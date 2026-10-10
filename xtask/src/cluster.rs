//! `./dev cluster init|up|status|down|kill|restart`: the local five-node launcher (PLAN.md
//! O-2, §7.7 step 0, D-63, W22.1, W22.2).
//!
//! **Init** generates one random 128-bit instance ID and stamps it into all five data
//! directories through `keyval-server init`, once. Before it touches anything it judges
//! every directory: one that holds more than an interrupted `init` can leave is refused,
//! and nothing is modified. The ID is recorded in the state directory *before* the first
//! stamp, so an `init` the machine interrupted resumes with the same ID: a directory
//! already stamped with it is accepted, one stamped with any other ID is refused untouched.
//! Once all five are stamped the record says so, and a further `init` is refused: only the
//! owner re-initialises a directory that ever held data.
//!
//! **Up** refuses an uninitialised directory or an occupied port before it starts any
//! process, starts each node detached in its own process group, and writes a PID receipt
//! for it at once. A node that exits during the start-up window rolls back the nodes this
//! `up` started, through the `Child` handles it still owns.
//!
//! **PID receipts** (`node-N.pid.json` in the state directory) name the process by more than
//! its PID: its start time, executable and command line. A process is "ours" only while
//! `/proc` still shows exactly those, so a recycled PID is never signalled (O-2). Nothing
//! here signals by port or by name, and nothing deletes a data directory.
//!
//! **Status** reports each node without changing anything. **Down** sends SIGTERM to the
//! nodes whose receipts still match, waits for them, and removes their receipts.
//!
//! **Kill** sends SIGKILL to one node, the way a crash probe stops it, and only if its
//! receipt still matches: a node that is not running, or whose PID now belongs to another
//! process, is an error and nothing is signalled. **Restart** kills the node if it runs and
//! starts it again, but only after checking that it can start (a completed init, a journal,
//! free ports); a refused restart leaves a running node running. Neither touches a data
//! directory, and a killed node's report carries its old PID in `detail`, not in `pid`.

// check: CLUSTER-LIFECYCLE

use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::output::Report;
use crate::process;

/// The `--json` report's identifier; its JSON Schema is `schemas/dev/cluster.v1.schema.json`.
pub const SCHEMA: &str = "keyval.dev.cluster/v1";

/// Most nodes a config may list (CD-9). The plan's cluster has five.
const MAX_NODES: usize = 16;

/// Most bytes read from a config, a record or a receipt (CD-9).
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Most bytes of a node's stderr quoted in a report (CD-9).
const MAX_TAIL_BYTES: usize = 2048;

/// How long `keyval-server init` may run.
const INIT_LIMIT: Duration = Duration::from_mins(1);

/// How often a wait polls `/proc` and the children.
const POLL: Duration = Duration::from_millis(50);

/// The file in the state directory that records the instance ID and the init phase.
const INSTANCE_RECORD: &str = "instance.json";

/// The journal file inside a data directory (`fs_storage::JOURNAL`; xtask depends on no
/// first-party crate).
const JOURNAL: &str = "journal.log";

/// The lock file inside a data directory (`fs_storage::LOCK`).
const LOCK: &str = "LOCK";

/// The `./dev cluster` action a report answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Generate the instance ID and stamp the data directories.
    Init,
    /// Start the nodes.
    Up,
    /// Report the nodes.
    Status,
    /// Stop the nodes.
    Down,
    /// SIGKILL one node, as a crash probe does.
    Kill,
    /// SIGKILL one node if it runs, then start it again.
    Restart,
}

impl Action {
    const fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Up => "up",
            Self::Status => "status",
            Self::Down => "down",
            Self::Kill => "kill",
            Self::Restart => "restart",
        }
    }
}

/// What a node is, or what the command did to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// The data directory holds no genesis.
    Uninitialised,
    /// The data directory holds a journal and no receipt names a live process.
    Initialised,
    /// `init` stamped the directory with the cluster instance ID.
    Stamped,
    /// A process this launcher started is running.
    Running,
    /// `up` started the process.
    Started,
    /// `down` stopped the process.
    Stopped,
    /// `kill` or `restart` killed the process.
    Killed,
    /// A receipt exists, but the process it names is gone or is not the one it recorded.
    Stale,
    /// The command refused this node and changed nothing in it.
    Refused,
}

/// One node of a report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeReport {
    /// The node's ID in the config.
    pub node: u64,
    /// Its data directory, resolved against the config's directory.
    pub data_dir: String,
    /// What it is, or what the command did.
    pub state: NodeState,
    /// The PID of its running process, if one is.
    pub pid: Option<u32>,
    /// Why, for people.
    pub detail: Option<String>,
}

/// The `keyval.dev.cluster/v1` report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClusterReport {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// Whether the command did what it was asked.
    pub ok: bool,
    /// The command.
    pub action: Action,
    /// The config file.
    pub config: String,
    /// The directory holding the instance record and the PID receipts.
    pub state_dir: String,
    /// The cluster instance ID, once known.
    pub instance_id: Option<String>,
    /// Every node of the config, in config order.
    pub nodes: Vec<NodeReport>,
    /// Why the command failed; `None` exactly when `ok`.
    pub error: Option<String>,
}

impl Report for ClusterReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut out = format!(
            "cluster {}: {} ({})\n",
            self.action.name(),
            if self.ok { "ok" } else { "FAILED" },
            self.config
        );
        if let Some(instance) = &self.instance_id {
            let _ = writeln!(out, "instance {instance}");
        }
        for node in &self.nodes {
            let _ = write!(
                out,
                "  node {}: {:?} {}",
                node.node, node.state, node.data_dir
            );
            if let Some(pid) = node.pid {
                let _ = write!(out, " pid {pid}");
            }
            if let Some(detail) = &node.detail {
                let _ = write!(out, " — {detail}");
            }
            out.push('\n');
        }
        if let Some(error) = &self.error {
            let _ = writeln!(out, "error: {error}");
        }
        out
    }
}

/// Where a cluster's files are.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The config file, absolute.
    pub config: PathBuf,
    /// The instance record and the PID receipts.
    pub state_dir: PathBuf,
}

impl Layout {
    /// The layout for `config`; the state directory defaults to
    /// `<root>/.local/cluster/<config stem>-<8 hex digits of the config path's hash>`, so
    /// two configs never share receipts and a per-run gate config passes its own.
    #[must_use]
    pub fn new(root: &Path, config: PathBuf, state_dir: Option<PathBuf>) -> Self {
        let state_dir = state_dir.map_or_else(
            || {
                let stem = config
                    .file_stem()
                    .map_or_else(|| "config".to_owned(), |s| s.to_string_lossy().into_owned());
                // Hash the canonical path, so `./a.toml`, `a.toml` and a symlink to it share
                // one state directory; a config that does not resolve hashes as spelled.
                let resolved = fs::canonicalize(&config).unwrap_or_else(|_| config.clone());
                let digest = Sha256::digest(resolved.to_string_lossy().as_bytes());
                root.join(".local/cluster")
                    .join(format!("{stem}-{}", hex(&digest[..4])))
            },
            |dir| root.join(dir),
        );
        Self { config, state_dir }
    }

    fn receipt(&self, node: u64) -> PathBuf {
        self.state_dir.join(format!("node-{node}.pid.json"))
    }

    fn stderr(&self, node: u64) -> PathBuf {
        self.state_dir.join(format!("node-{node}.stderr"))
    }

    fn record(&self) -> PathBuf {
        self.state_dir.join(INSTANCE_RECORD)
    }

    fn report(&self, action: Action) -> ClusterReport {
        ClusterReport {
            schema: SCHEMA,
            ok: true,
            action,
            config: self.config.display().to_string(),
            state_dir: self.state_dir.display().to_string(),
            instance_id: None,
            nodes: Vec::new(),
            error: None,
        }
    }
}

/// How long the waits last.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// How long `up` watches the nodes it started for an early exit.
    pub grace: Duration,
    /// How long `down` waits for a node to exit after SIGTERM.
    pub stop: Duration,
}

impl Timing {
    /// What `./dev cluster` uses.
    pub const LIVE: Self = Self {
        grace: Duration::from_millis(1500),
        stop: Duration::from_secs(15),
    };
}

/// One node of the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    /// The node ID.
    pub id: u64,
    /// The data directory, resolved against the config's directory (§7.7 step 0).
    pub data_dir: PathBuf,
    /// The client API address.
    pub client: SocketAddr,
    /// The peer address.
    pub peer: SocketAddr,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{} is larger than {MAX_FILE_BYTES} bytes",
            path.display()
        ));
    }
    Ok(text)
}

/// Writes `text` to `path` through a rename, so a reader sees all of it or none.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let partial = path.with_extension("partial");
    let write = || -> std::io::Result<()> {
        let mut file = fs::File::create(&partial)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&partial, path)?;
        // Make the rename itself durable.
        match path.parent() {
            Some(dir) => fs::File::open(dir)?.sync_all(),
            None => Ok(()),
        }
    };
    write().map_err(|err| format!("{}: {err}", path.display()))
}

/// The nodes of the config at `path`: the launcher reads only what it needs, and
/// `keyval-server` validates the rest.
///
/// # Errors
///
/// The file cannot be read or parsed, lists no node or more than [`MAX_NODES`], repeats an
/// ID, or has a node without an ID, a data directory or a valid address.
pub fn load_config(path: &Path) -> Result<Vec<NodeConfig>, String> {
    let text = read_bounded(path)?;
    let value: toml::Table = text
        .parse()
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let tables = value
        .get("nodes")
        .and_then(toml::Value::as_array)
        .filter(|nodes| !nodes.is_empty())
        .ok_or_else(|| format!("{}: no [[nodes]]", path.display()))?;
    if tables.len() > MAX_NODES {
        return Err(format!(
            "{}: {} nodes; the launcher manages at most {MAX_NODES}",
            path.display(),
            tables.len()
        ));
    }
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut nodes: Vec<NodeConfig> = Vec::new();
    for table in tables {
        let text = |key: &str| {
            table
                .get(key)
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("{}: a node lacks a string `{key}`", path.display()))
        };
        let id = table
            .get("id")
            .and_then(toml::Value::as_integer)
            .and_then(|id| u64::try_from(id).ok())
            .filter(|id| *id > 0)
            .ok_or_else(|| format!("{}: a node lacks a positive integer `id`", path.display()))?;
        if nodes.iter().any(|node| node.id == id) {
            return Err(format!("{}: node {id} is listed twice", path.display()));
        }
        let address = |key: &str| {
            text(key).and_then(|value| {
                value
                    .parse::<SocketAddr>()
                    .map_err(|err| format!("{}: node {id} {key} {value:?}: {err}", path.display()))
            })
        };
        nodes.push(NodeConfig {
            id,
            data_dir: dir.join(text("data_dir")?),
            client: address("client_address")?,
            peer: address("peer_address")?,
        });
    }
    Ok(nodes)
}

/// A random 128-bit instance ID as 32 lowercase hex digits, from the operating system.
///
/// # Errors
///
/// `/dev/urandom` cannot be read.
pub fn random_instance_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|err| format!("/dev/urandom: {err}"))?;
    Ok(hex(&bytes))
}

// --- Processes ------------------------------------------------------------------------

/// What `/proc` shows of a process; the receipt records the same, so a recycled PID differs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    start_ticks: u64,
    exe: String,
    cmdline: Vec<String>,
}

/// The identity of `pid`, or `None` if it does not exist or has exited (a zombie counts as
/// exited: it runs nothing and holds no port).
fn identity(pid: u32) -> Option<Identity> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let stat = fs::read_to_string(proc.join("stat")).ok()?;
    // The command name is parenthesised and may itself hold ')' or spaces: fields resume
    // after the last ')'. Field 3 (state) is the first of them; field 22 (start time) the 20th.
    let after = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    if ["Z", "X"].contains(fields.first()?) {
        return None;
    }
    let start_ticks = fields.get(19)?.parse().ok()?;
    // A rebuilt binary replaces the file a live node runs: the kernel then appends this
    // suffix. It is the same process, so the suffix is not part of its identity.
    let exe = fs::read_link(proc.join("exe")).ok()?.display().to_string();
    let exe = exe
        .strip_suffix(" (deleted)")
        .map_or_else(|| exe.clone(), str::to_owned);
    let raw = fs::read(proc.join("cmdline")).ok()?;
    let cmdline = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    Some(Identity {
        start_ticks,
        exe,
        cmdline,
    })
}

/// A PID receipt: the process this launcher started for a node.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Receipt {
    pid: u32,
    identity: Identity,
}

impl Receipt {
    fn to_json(&self, node: &NodeConfig, config: &Path) -> Value {
        json!({
            "node": node.id,
            "pid": self.pid,
            "start_ticks": self.identity.start_ticks,
            "exe": self.identity.exe,
            "cmdline": self.identity.cmdline,
            "config": config.display().to_string(),
            "data_dir": node.data_dir.display().to_string(),
        })
    }

    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let cmdline = value["cmdline"]
            .as_array()?
            .iter()
            .map(|part| part.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            pid: u32::try_from(value["pid"].as_u64()?).ok()?,
            identity: Identity {
                start_ticks: value["start_ticks"].as_u64()?,
                exe: value["exe"].as_str()?.to_owned(),
                cmdline,
            },
        })
    }
}

/// Whether the process a receipt names is the one it recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owned {
    /// The same process: started by this launcher and still running.
    Ours,
    /// No such process any more.
    Gone,
    /// The PID now belongs to a different process.
    Foreign,
}

fn owned(receipt: &Receipt) -> Owned {
    match identity(receipt.pid) {
        None => Owned::Gone,
        Some(now) if now == receipt.identity => Owned::Ours,
        Some(_) => Owned::Foreign,
    }
}

/// The receipt at `path`, `None` if there is none.
fn read_receipt(path: &Path) -> Result<Option<Receipt>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = read_bounded(path)?;
    Receipt::parse(&text)
        .map(Some)
        .ok_or_else(|| format!("{} is not a PID receipt; left as it is", path.display()))
}

/// A signal the launcher sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    /// A request to exit, which the server answers by shutting down.
    Term,
    /// An exit the process cannot answer: a crash, for the probes.
    Kill,
}

impl Signal {
    const fn flag(self) -> &'static str {
        match self {
            Self::Term => "-TERM",
            Self::Kill => "-KILL",
        }
    }
}

/// Sends `signal` to `pid`. The caller has just checked that the PID is its own process.
fn terminate(pid: u32, signal: Signal) -> Result<(), String> {
    let flag = signal.flag();
    let captured = process::run(
        Command::new("kill").args([flag, &pid.to_string()]),
        Duration::from_secs(10),
    )
    .map_err(|err| format!("kill {flag} {pid}: {err}"))?;
    if captured.status.success() {
        Ok(())
    } else {
        Err(format!(
            "kill {flag} {pid} exited with {}: {}",
            captured.status,
            tail(&captured.stderr)
        ))
    }
}

/// The end of `text`, at a character boundary, at most [`MAX_TAIL_BYTES`] long.
fn tail(text: &str) -> String {
    let text = text.trim();
    let mut start = text.len().saturating_sub(MAX_TAIL_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

// --- Data directories -----------------------------------------------------------------

/// Whether the directory holds a journal with content: initialised as far as the launcher
/// can see. `keyval-server run` judges the genesis itself.
fn looks_initialised(dir: &Path) -> bool {
    fs::metadata(dir.join(JOURNAL)).is_ok_and(|journal| journal.len() > 0)
}

/// `None` if `dir` is fresh for `init`: absent, empty, or holding only an empty journal
/// and an empty lock file. Otherwise what it holds.
fn used_for_init(dir: &Path) -> Option<String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => return Some(format!("cannot be read: {err}")),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => return Some(format!("cannot be listed: {err}")),
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let empty = entry
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.len() == 0);
        if ![JOURNAL, LOCK].contains(&name.as_str()) || !empty {
            return Some(format!(
                "holds {name:?}: it has been used, or an `init` outside this launcher was \
                 interrupted in it"
            ));
        }
    }
    None
}

fn node_report(node: &NodeConfig, state: NodeState) -> NodeReport {
    NodeReport {
        node: node.id,
        data_dir: node.data_dir.display().to_string(),
        state,
        pid: None,
        detail: None,
    }
}

fn failed(mut report: ClusterReport, error: impl Into<String>) -> ClusterReport {
    report.ok = false;
    report.error = Some(error.into());
    report
}

// --- The instance record --------------------------------------------------------------

/// Where `init` is: the ID is recorded before the first stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InstanceRecord {
    instance_id: String,
    nodes: Vec<u64>,
    complete: bool,
}

impl InstanceRecord {
    fn to_text(&self) -> String {
        json!({
            "version": 1,
            "instance_id": self.instance_id,
            "nodes": self.nodes,
            "phase": if self.complete { "initialised" } else { "stamping" },
        })
        .to_string()
    }

    fn read(layout: &Layout) -> Result<Option<Self>, String> {
        let path = layout.record();
        if !path.exists() {
            return Ok(None);
        }
        let text = read_bounded(&path)?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| format!("{} is not JSON ({err}); left as it is", path.display()))?;
        let parsed = (|| {
            let nodes = value["nodes"]
                .as_array()?
                .iter()
                .map(Value::as_u64)
                .collect::<Option<Vec<_>>>()?;
            let complete = match value["phase"].as_str()? {
                "initialised" => true,
                "stamping" => false,
                _ => return None,
            };
            Some(Self {
                instance_id: value["instance_id"].as_str()?.to_owned(),
                nodes,
                complete,
            })
        })();
        parsed
            .map(Some)
            .ok_or_else(|| format!("{} is malformed; left as it is", path.display()))
    }
}

/// The instance ID in a `keyval-server` "initialised already" message.
fn instance_in(message: &str) -> Option<&str> {
    let start = message.find("(instance ")? + "(instance ".len();
    let id = message.get(start..start + 32)?;
    id.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        .then_some(id)
}

// --- init -----------------------------------------------------------------------------

/// What `init` decided from the record and the directories, before touching any.
struct InitPlan {
    nodes: Vec<NodeConfig>,
    ids: Vec<u64>,
    /// The ID of the interrupted init this one resumes, if there is one.
    resuming: Option<String>,
}

/// Judges the record and every data directory: `Err` is the refusal, nothing modified.
fn plan_init(layout: &Layout, report: &ClusterReport) -> Result<InitPlan, Box<ClusterReport>> {
    let nodes = load_config(&layout.config).map_err(|why| Box::new(failed(report.clone(), why)))?;
    let ids: Vec<u64> = nodes.iter().map(|node| node.id).collect();
    let record =
        InstanceRecord::read(layout).map_err(|why| Box::new(failed(report.clone(), why)))?;
    if let Some(record) = &record {
        if record.complete {
            let mut refused = failed(
                report.clone(),
                format!(
                    "the cluster is initialised already (instance {}); re-initialising a \
                     directory that holds data is the owner's decision, never `init`'s",
                    record.instance_id
                ),
            );
            refused.instance_id = Some(record.instance_id.clone());
            return Err(Box::new(refused));
        }
        if record.nodes != ids {
            return Err(Box::new(failed(
                report.clone(),
                format!(
                    "an interrupted init of nodes {:?} is recorded in {}, but the config \
                     lists {ids:?}",
                    record.nodes,
                    layout.record().display()
                ),
            )));
        }
    }
    let resuming = record.map(|record| record.instance_id);
    {
        // A fresh init takes only fresh directories. A resumed one also takes those it
        // stamped (they hold a journal); stray content without one is refused up front. Which
        // instance a stamped directory holds is the server's to say, at stamp time.
        let resumed = resuming.is_some();
        let used: Vec<NodeReport> = nodes
            .iter()
            .filter(|node| !resumed || !looks_initialised(&node.data_dir))
            .filter_map(|node| {
                used_for_init(&node.data_dir).map(|why| NodeReport {
                    detail: Some(why),
                    ..node_report(node, NodeState::Refused)
                })
            })
            .collect();
        if !used.is_empty() {
            let mut refused = failed(
                report.clone(),
                format!(
                    "{} data director{} used already; nothing was modified",
                    used.len(),
                    if used.len() == 1 { "y is" } else { "ies are" }
                ),
            );
            refused.nodes = used;
            return Err(Box::new(refused));
        }
    }
    Ok(InitPlan {
        nodes,
        ids,
        resuming,
    })
}

/// `./dev cluster init`: see the module docs.
pub fn init(layout: &Layout, server: &Path) -> ClusterReport {
    let report = layout.report(Action::Init);
    let InitPlan {
        nodes,
        ids,
        resuming,
    } = match plan_init(layout, &report) {
        Ok(plan) => plan,
        Err(refused) => return *refused,
    };
    let was_resuming = resuming.is_some();
    let instance_id = match resuming {
        Some(id) => id,
        None => match random_instance_id() {
            Ok(id) => id,
            Err(why) => return failed(report, why),
        },
    };
    let mut record = InstanceRecord {
        instance_id: instance_id.clone(),
        nodes: ids,
        complete: false,
    };
    let recorded = fs::create_dir_all(&layout.state_dir)
        .map_err(|err| format!("{}: {err}", layout.state_dir.display()))
        .and_then(|()| write_atomic(&layout.record(), &record.to_text()));
    let mut report = layout.report(Action::Init);
    report.instance_id = Some(instance_id.clone());
    if let Err(why) = recorded {
        return failed(report, why);
    }
    for node in &nodes {
        if let Some(parent) = node.data_dir.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            report.nodes.push(NodeReport {
                detail: Some(format!("{}: {err}", parent.display())),
                ..node_report(node, NodeState::Refused)
            });
            return failed(
                report,
                format!("node {}: its parent directory failed", node.id),
            );
        }
        match stamp(layout, server, node, &instance_id, was_resuming) {
            Ok(done) => report.nodes.push(done),
            Err(refused) => {
                let error = format!(
                    "node {} refused: {}",
                    node.id,
                    refused.detail.as_deref().unwrap_or("no reason given")
                );
                report.nodes.push(refused);
                for rest in nodes.iter().skip(report.nodes.len()) {
                    report.nodes.push(NodeReport {
                        detail: Some("not attempted".to_owned()),
                        ..node_report(rest, NodeState::Uninitialised)
                    });
                }
                return failed(report, error);
            }
        }
    }
    record.complete = true;
    match write_atomic(&layout.record(), &record.to_text()) {
        Ok(()) => report,
        Err(why) => failed(report, why),
    }
}

/// Runs `keyval-server init` for `node`. A directory already stamped with `instance` is
/// accepted only when `resuming`.
fn stamp(
    layout: &Layout,
    server: &Path,
    node: &NodeConfig,
    instance: &str,
    resuming: bool,
) -> Result<NodeReport, NodeReport> {
    let refuse = |detail: String| NodeReport {
        detail: Some(detail),
        ..node_report(node, NodeState::Refused)
    };
    let mut cmd = Command::new(server);
    cmd.args(["init", "--config"]).arg(&layout.config).args([
        "--node",
        &node.id.to_string(),
        "--instance",
        instance,
        "--json",
    ]);
    let captured = process::run(&mut cmd, INIT_LIMIT)
        .map_err(|err| refuse(format!("keyval-server init: {err}")))?;
    let Ok(answer) = serde_json::from_str::<Value>(captured.stdout.trim()) else {
        return Err(refuse(format!(
            "keyval-server init printed no JSON (exit {}): {}",
            captured.status,
            tail(&captured.stderr)
        )));
    };
    if captured.status.success() {
        return if answer["schema"] == "keyval.server.init/v1" && answer["instance_id"] == instance {
            Ok(node_report(node, NodeState::Stamped))
        } else {
            Err(refuse(format!("keyval-server init reported {answer}")))
        };
    }
    let message = answer["message"].as_str().unwrap_or_default();
    if resuming && answer["code"] == "already_initialised" && instance_in(message) == Some(instance)
    {
        return Ok(NodeReport {
            detail: Some("stamped with this instance by the interrupted init".to_owned()),
            ..node_report(node, NodeState::Stamped)
        });
    }
    Err(refuse(format!(
        "{} ({})",
        message,
        answer["code"].as_str().unwrap_or("no code")
    )))
}

// --- status ---------------------------------------------------------------------------

fn instance_of(layout: &Layout) -> Option<String> {
    InstanceRecord::read(layout)
        .ok()
        .flatten()
        .map(|record| record.instance_id)
}

/// `./dev cluster status`: changes nothing.
pub fn status(layout: &Layout) -> ClusterReport {
    let mut report = layout.report(Action::Status);
    let nodes = match load_config(&layout.config) {
        Ok(nodes) => nodes,
        Err(why) => return failed(report, why),
    };
    report.instance_id = instance_of(layout);
    let mut unreadable = Vec::new();
    for node in &nodes {
        let mut entry = node_report(node, NodeState::Uninitialised);
        match read_receipt(&layout.receipt(node.id)) {
            Ok(Some(receipt)) => match owned(&receipt) {
                Owned::Ours => {
                    entry.state = NodeState::Running;
                    entry.pid = Some(receipt.pid);
                }
                Owned::Gone => {
                    entry.state = NodeState::Stale;
                    entry.detail = Some(format!("pid {} is gone", receipt.pid));
                }
                Owned::Foreign => {
                    entry.state = NodeState::Stale;
                    entry.detail = Some(format!(
                        "pid {} now belongs to another process; it is never signalled",
                        receipt.pid
                    ));
                }
            },
            Ok(None) => {
                if looks_initialised(&node.data_dir) {
                    entry.state = NodeState::Initialised;
                }
            }
            Err(why) => {
                unreadable.push(why.clone());
                entry.detail = Some(why);
                entry.state = NodeState::Refused;
            }
        }
        report.nodes.push(entry);
    }
    if unreadable.is_empty() {
        report
    } else {
        failed(report, unreadable.join("; "))
    }
}

// --- up -------------------------------------------------------------------------------

/// A node `up` started, with the handle that proves the PID is still its own.
struct Started {
    node: NodeConfig,
    child: Child,
}

/// What `up` found out about one node before starting anything.
enum UpJudgement {
    /// Its receipt names a live process of ours.
    Running(NodeReport),
    /// It cannot be started; the report says why.
    Refused(NodeReport),
    /// It may be started.
    Start(NodeReport),
}

fn judge_up(layout: &Layout, node: &NodeConfig) -> UpJudgement {
    let mut entry = node_report(node, NodeState::Initialised);
    let refused = |state, detail: String| {
        UpJudgement::Refused(NodeReport {
            detail: Some(detail),
            ..node_report(node, state)
        })
    };
    let found = match read_receipt(&layout.receipt(node.id)) {
        Err(why) => return refused(NodeState::Refused, why),
        Ok(Some(receipt)) if owned(&receipt) == Owned::Ours => {
            entry.state = NodeState::Running;
            entry.pid = Some(receipt.pid);
            entry.detail = Some("already running".to_owned());
            return UpJudgement::Running(entry);
        }
        Ok(found) => found,
    };
    if !looks_initialised(&node.data_dir) {
        return refused(
            NodeState::Uninitialised,
            format!(
                "{} holds no journal: not initialised; run `./dev cluster init`",
                node.data_dir.display()
            ),
        );
    }
    if let Some(taken) = [node.client, node.peer]
        .into_iter()
        .find(|address| TcpListener::bind(address).is_err())
    {
        return refused(
            NodeState::Refused,
            format!("{taken} is in use by another process"),
        );
    }
    if found.is_some() {
        entry.state = NodeState::Stale;
        entry.detail = Some("receipt of a process that is gone; removed".to_owned());
    }
    UpJudgement::Start(entry)
}

/// Watches the nodes `up` started for `grace`; `Some` is why one of them did not stay up.
fn watch(layout: &Layout, started: &mut [Started], grace: Duration) -> Option<String> {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        for entry in started.iter_mut() {
            if let Ok(Some(status)) = entry.child.try_wait() {
                return Some(format!(
                    "node {} exited with {status} during start-up: {}",
                    entry.node.id,
                    tail(&read_bounded(&layout.stderr(entry.node.id)).unwrap_or_default())
                ));
            }
        }
        thread::sleep(POLL);
    }
    None
}

/// The instance ID of the completed init, `None` if there is no record; `Err` if the record
/// is unreadable or names an interrupted init.
fn completed_record(layout: &Layout) -> Result<Option<String>, String> {
    match InstanceRecord::read(layout)? {
        Some(record) if !record.complete => Err(format!(
            "init was interrupted (instance {}): run `./dev cluster init` again first",
            record.instance_id
        )),
        record => Ok(record.map(|record| record.instance_id)),
    }
}

/// The nodes of the config `only` selects: all of them, or the one with that ID.
fn select(nodes: Vec<NodeConfig>, only: Option<u64>) -> Result<Vec<NodeConfig>, String> {
    let Some(id) = only else {
        return Ok(nodes);
    };
    let ids: Vec<u64> = nodes.iter().map(|node| node.id).collect();
    let chosen: Vec<NodeConfig> = nodes.into_iter().filter(|node| node.id == id).collect();
    if chosen.is_empty() {
        Err(format!("node {id} is not in the config (nodes {ids:?})"))
    } else {
        Ok(chosen)
    }
}

/// `./dev cluster up`: see the module docs.
pub fn up(layout: &Layout, server: &Path, timing: Timing) -> ClusterReport {
    start(layout, server, timing, Action::Up, None)
}

/// Starts the nodes `only` selects (all if `None`) and reports as `action`.
fn start(
    layout: &Layout,
    server: &Path,
    timing: Timing,
    action: Action,
    only: Option<u64>,
) -> ClusterReport {
    let mut report = layout.report(action);
    let nodes = match load_config(&layout.config).and_then(|nodes| select(nodes, only)) {
        Ok(nodes) => nodes,
        Err(why) => return failed(report, why),
    };
    match completed_record(layout) {
        Ok(id) => report.instance_id = id,
        Err(why) => return failed(report, why),
    }
    // Judge everything before starting anything.
    let mut refusals = Vec::new();
    let mut to_start = Vec::new();
    for node in &nodes {
        match judge_up(layout, node) {
            UpJudgement::Running(entry) => report.nodes.push(entry),
            UpJudgement::Refused(entry) => refusals.push(entry),
            UpJudgement::Start(entry) => to_start.push((node.clone(), entry)),
        }
    }
    if !refusals.is_empty() {
        report.nodes.extend(refusals);
        report.nodes.sort_by_key(|entry| entry.node);
        return failed(
            report,
            "refused before starting any node; nothing was started",
        );
    }
    if let Err(why) = fs::create_dir_all(&layout.state_dir) {
        return failed(report, format!("{}: {why}", layout.state_dir.display()));
    }
    let mut started: Vec<Started> = Vec::new();
    for (node, mut entry) in to_start {
        if let Err(why) = fs::remove_file(layout.receipt(node.id))
            && why.kind() != std::io::ErrorKind::NotFound
        {
            return roll_back(layout, report, started, format!("node {}: {why}", node.id));
        }
        match spawn(layout, server, &node) {
            Ok(child) => {
                entry.state = NodeState::Started;
                entry.pid = Some(child.id());
                entry.detail = None;
                report.nodes.push(entry);
                started.push(Started { node, child });
            }
            Err(why) => {
                return roll_back(layout, report, started, format!("node {}: {why}", node.id));
            }
        }
    }
    if let Some(why) = watch(layout, &mut started, timing.grace) {
        return roll_back(layout, report, started, why);
    }
    // The handles drop here; the nodes run on, and their receipts are their owners.
    report.nodes.sort_by_key(|entry| entry.node);
    report
}

/// The identity of the new process `pid` once `/proc` shows its final command line, which
/// names `run` and `config`: read while the process is still being exec'd, it can show the
/// launcher's own, and a receipt of that would never match the running server.
fn settled_identity(pid: u32, config: &Path) -> Option<Identity> {
    let config = config.display().to_string();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = identity(pid);
        let settled = found.as_ref().is_some_and(|now| {
            now.cmdline.iter().any(|arg| arg == "run") && now.cmdline.contains(&config)
        });
        if settled || found.is_none() || Instant::now() >= deadline {
            return found.filter(|_| settled);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Starts `server run` for `node` detached, and writes its receipt before returning.
fn spawn(layout: &Layout, server: &Path, node: &NodeConfig) -> Result<Child, String> {
    let stderr = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(layout.stderr(node.id))
        .map_err(|err| format!("{}: {err}", layout.stderr(node.id).display()))?;
    let mut child = Command::new(server)
        .args(["run", "--config"])
        .arg(&layout.config)
        .args(["--node", &node.id.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|err| format!("keyval-server run: {err}"))?;
    let receipt = settled_identity(child.id(), &layout.config).map(|identity| Receipt {
        pid: child.id(),
        identity,
    });
    let written = receipt
        .ok_or_else(|| {
            format!(
                "pid {} exited during start-up before its receipt was written",
                child.id()
            )
        })
        .and_then(|receipt| {
            write_atomic(
                &layout.receipt(node.id),
                &receipt.to_json(node, &layout.config).to_string(),
            )
        });
    if let Err(why) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(why);
    }
    Ok(child)
}

/// Stops the nodes this `up` started, through the handles it owns, removes their receipts
/// and fails the report with `why`.
fn roll_back(
    layout: &Layout,
    mut report: ClusterReport,
    started: Vec<Started>,
    why: String,
) -> ClusterReport {
    for mut entry in started {
        let _ = entry.child.kill();
        let _ = entry.child.wait();
        let _ = fs::remove_file(layout.receipt(entry.node.id));
        if let Some(shown) = report.nodes.iter_mut().find(|n| n.node == entry.node.id) {
            shown.state = NodeState::Stopped;
            shown.pid = None;
            shown.detail = Some("rolled back: another node failed to start".to_owned());
        }
    }
    report.nodes.sort_by_key(|entry| entry.node);
    failed(report, why)
}

// --- down -----------------------------------------------------------------------------

/// `./dev cluster down`: SIGTERM to every node whose receipt still matches its process.
pub fn down(layout: &Layout, timing: Timing) -> ClusterReport {
    let mut report = layout.report(Action::Down);
    let nodes = match load_config(&layout.config) {
        Ok(nodes) => nodes,
        Err(why) => return failed(report, why),
    };
    report.instance_id = instance_of(layout);
    let mut errors = Vec::new();
    for node in &nodes {
        let initial = if looks_initialised(&node.data_dir) {
            NodeState::Initialised
        } else {
            NodeState::Uninitialised
        };
        let mut entry = node_report(node, initial);
        let path = layout.receipt(node.id);
        match read_receipt(&path) {
            Err(why) => {
                errors.push(why.clone());
                entry.state = NodeState::Refused;
                entry.detail = Some(why);
            }
            Ok(None) => entry.detail = Some("not started by this launcher".to_owned()),
            Ok(Some(receipt)) => match owned(&receipt) {
                Owned::Ours => match stop(&receipt, Signal::Term, timing.stop) {
                    Ok(()) => {
                        entry.state = NodeState::Stopped;
                        if let Err(err) = fs::remove_file(&path) {
                            errors.push(format!("{}: {err}", path.display()));
                        }
                    }
                    Err(why) => {
                        errors.push(format!("node {}: {why}", node.id));
                        entry.state = NodeState::Running;
                        entry.pid = Some(receipt.pid);
                        entry.detail = Some(why);
                    }
                },
                Owned::Gone | Owned::Foreign => {
                    entry.state = NodeState::Stale;
                    entry.detail = Some(format!(
                        "pid {} is not the process the receipt recorded: nothing signalled, \
                         receipt removed",
                        receipt.pid
                    ));
                    if let Err(err) = fs::remove_file(&path) {
                        errors.push(format!("{}: {err}", path.display()));
                    }
                }
            },
        }
        report.nodes.push(entry);
    }
    if errors.is_empty() {
        report
    } else {
        failed(report, errors.join("; "))
    }
}

// --- kill and restart -----------------------------------------------------------------

/// What `kill` found at a node's receipt.
enum Killed {
    /// The process was the receipted one; it was sent SIGKILL and is gone.
    Dead(NodeReport),
    /// No receipt, or the receipted process had already exited.
    NotRunning(NodeReport),
    /// The PID now belongs to another process: nothing was signalled.
    Foreign(NodeReport),
    /// The receipt is unreadable, or the process outlived the signal.
    Failed(NodeReport, String),
}

/// SIGKILLs `node` if, and only if, its receipt still names the process it recorded.
/// A receipt that no longer names a live process of ours is removed; a data directory is
/// never touched.
fn kill_node(layout: &Layout, node: &NodeConfig, limit: Duration) -> Killed {
    let initial = if looks_initialised(&node.data_dir) {
        NodeState::Initialised
    } else {
        NodeState::Uninitialised
    };
    let mut entry = node_report(node, initial);
    let path = layout.receipt(node.id);
    let clear = |entry: &mut NodeReport| {
        if let Err(err) = fs::remove_file(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            entry.detail = Some(format!("{}: {err}", path.display()));
        }
    };
    let receipt = match read_receipt(&path) {
        Err(why) => {
            entry.state = NodeState::Refused;
            entry.detail = Some(why.clone());
            return Killed::Failed(entry, why);
        }
        Ok(None) => {
            entry.detail = Some("not started by this launcher".to_owned());
            return Killed::NotRunning(entry);
        }
        Ok(Some(receipt)) => receipt,
    };
    match owned(&receipt) {
        Owned::Ours => match stop(&receipt, Signal::Kill, limit) {
            Ok(()) => {
                entry.state = NodeState::Killed;
                entry.detail = Some(format!("pid {} killed", receipt.pid));
                clear(&mut entry);
                Killed::Dead(entry)
            }
            // It may have exited on its own before the signal landed: look again.
            Err(_) if owned(&receipt) == Owned::Gone => {
                entry.state = NodeState::Stale;
                entry.detail = Some(format!(
                    "pid {} exited before the signal; receipt removed",
                    receipt.pid
                ));
                clear(&mut entry);
                Killed::NotRunning(entry)
            }
            Err(why) => {
                entry.state = NodeState::Running;
                entry.pid = Some(receipt.pid);
                entry.detail = Some(why.clone());
                Killed::Failed(entry, why)
            }
        },
        Owned::Gone => {
            entry.state = NodeState::Stale;
            entry.detail = Some(format!(
                "pid {} had exited already; receipt removed",
                receipt.pid
            ));
            clear(&mut entry);
            Killed::NotRunning(entry)
        }
        Owned::Foreign => {
            entry.state = NodeState::Refused;
            entry.detail = Some(format!(
                "pid {} is not the process the receipt recorded: nothing signalled, \
                 receipt removed",
                receipt.pid
            ));
            clear(&mut entry);
            Killed::Foreign(entry)
        }
    }
}

/// `./dev cluster kill --node N`: SIGKILL, the way a crash probe stops a node.
pub fn kill(layout: &Layout, node: u64, timing: Timing) -> ClusterReport {
    let mut report = layout.report(Action::Kill);
    let nodes = match load_config(&layout.config).and_then(|nodes| select(nodes, Some(node))) {
        Ok(nodes) => nodes,
        Err(why) => return failed(report, why),
    };
    report.instance_id = instance_of(layout);
    let error = match kill_node(layout, &nodes[0], timing.stop) {
        Killed::Dead(entry) => {
            report.nodes.push(entry);
            return report;
        }
        Killed::NotRunning(entry) => {
            report.nodes.push(entry);
            format!("node {node} is not running; nothing was signalled")
        }
        Killed::Foreign(entry) => {
            report.nodes.push(entry);
            format!("node {node}: its pid now belongs to another process; nothing was signalled")
        }
        Killed::Failed(entry, why) => {
            report.nodes.push(entry);
            format!("node {node}: {why}")
        }
    };
    failed(report, error)
}

/// `./dev cluster restart --node N`: SIGKILL the node if it runs, then start it again,
/// with `up`'s refusals for an uninitialised directory or an occupied port.
pub fn restart(layout: &Layout, server: &Path, timing: Timing, node: u64) -> ClusterReport {
    let mut report = layout.report(Action::Restart);
    let nodes = match load_config(&layout.config).and_then(|nodes| select(nodes, Some(node))) {
        Ok(nodes) => nodes,
        Err(why) => return failed(report, why),
    };
    // Refuse before killing what cannot be started again. A live node's own ports are free
    // once it dies, so they are probed only when no process of ours holds them.
    let live = matches!(
        read_receipt(&layout.receipt(node)),
        Ok(Some(receipt)) if owned(&receipt) == Owned::Ours
    );
    let blocked = completed_record(layout).err().or_else(|| {
        if !looks_initialised(&nodes[0].data_dir) {
            Some(format!(
                "{} holds no journal: not initialised",
                nodes[0].data_dir.display()
            ))
        } else if live {
            None
        } else {
            [nodes[0].client, nodes[0].peer]
                .into_iter()
                .find(|address| TcpListener::bind(address).is_err())
                .map(|taken| format!("{taken} is in use by another process"))
        }
    });
    if let Some(why) = blocked {
        report.instance_id = instance_of(layout);
        report.nodes.push(NodeReport {
            detail: Some(why.clone()),
            ..node_report(&nodes[0], NodeState::Refused)
        });
        return failed(
            report,
            format!("node {node} refused before anything was signalled: {why}"),
        );
    }
    let before = match kill_node(layout, &nodes[0], timing.stop) {
        Killed::Failed(entry, why) => {
            report.instance_id = instance_of(layout);
            report.nodes.push(entry);
            return failed(report, format!("node {node}: {why}"));
        }
        Killed::Dead(entry) | Killed::NotRunning(entry) | Killed::Foreign(entry) => entry.detail,
    };
    let mut started = start(layout, server, timing, Action::Restart, Some(node));
    // A refusal after the kill must not read as "untouched": say what was done first.
    // A start that failed before any node was listed (the process died before its receipt
    // was written) still has the killed node to show.
    if started.nodes.is_empty() {
        started
            .nodes
            .push(node_report(&nodes[0], NodeState::Killed));
    }
    if let (Some(before), Some(shown)) = (before, started.nodes.first_mut()) {
        shown.detail = Some(match shown.detail.take() {
            Some(then) => format!("{before}; then {then}"),
            None => before,
        });
    }
    started
}

/// Signals the process of `receipt` and waits until it is gone.
fn stop(receipt: &Receipt, signal: Signal, limit: Duration) -> Result<(), String> {
    terminate(receipt.pid, signal)?;
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if owned(receipt) != Owned::Ours {
            return Ok(());
        }
        thread::sleep(POLL);
    }
    Err(format!(
        "pid {} still runs {}s after {}; it is left running",
        receipt.pid,
        limit.as_secs(),
        signal.flag()
    ))
}

// --- The server binary ----------------------------------------------------------------

/// Builds `keyval-server` and returns its path.
///
/// # Errors
///
/// The build fails or names no executable.
pub fn build_server(root: &Path) -> Result<PathBuf, String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args([
        "build",
        "--locked",
        "--quiet",
        "--message-format=json",
        "-p",
        "server",
        "--bin",
        "keyval-server",
    ]);
    let captured = process::run(&mut cmd, Duration::from_mins(30))
        .map_err(|err| format!("cargo build: {err}"))?;
    if !captured.status.success() {
        return Err(format!(
            "cargo build exited with {}: {}",
            captured.status,
            tail(&captured.stderr)
        ));
    }
    captured
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact" && message["target"]["name"] == "keyval-server"
        })
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .ok_or_else(|| "cargo built no keyval-server executable".to_owned())
}

/// Exclusive use of a state directory by one launcher invocation. The lock file names its
/// holder's identity, so a lock left by a crashed launcher is recognised and replaced, and a
/// live holder's never is.
struct Lock {
    path: PathBuf,
}

impl Lock {
    fn take(layout: &Layout) -> Result<Self, String> {
        fs::create_dir_all(&layout.state_dir)
            .map_err(|err| format!("{}: {err}", layout.state_dir.display()))?;
        let path = layout.state_dir.join("lock.json");
        let me = std::process::id();
        let ticks = identity(me).map_or(0, |id| id.start_ticks);
        let body = json!({"pid": me, "start_ticks": ticks}).to_string();
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(body.as_bytes())
                        .map_err(|err| format!("{}: {err}", path.display()))?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder: Value = read_bounded(&path)
                        .ok()
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or(Value::Null);
                    let live = holder["pid"].as_u64().is_some_and(|pid| {
                        u32::try_from(pid)
                            .ok()
                            .and_then(identity)
                            .is_some_and(|id| {
                                Some(id.start_ticks) == holder["start_ticks"].as_u64()
                            })
                    });
                    if live {
                        return Err(format!(
                            "another `cluster` command (pid {}) holds {}",
                            holder["pid"],
                            path.display()
                        ));
                    }
                    // The holder is gone: its lock is stale and is ours to replace.
                    fs::remove_file(&path).map_err(|err| format!("{}: {err}", path.display()))?;
                }
                Err(err) => return Err(format!("{}: {err}", path.display())),
            }
        }
        Err(format!("{} is contended; try again", path.display()))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Best effort: a lock that stays is judged stale by the next command.
        drop(fs::remove_file(&self.path));
    }
}

/// Runs `action` against the config at `layout`, building the server for `init`, `up` and
/// `restart`. `kill` and `restart` act on `node`, which they require.
pub fn run(root: &Path, action: Action, layout: &Layout, node: Option<u64>) -> ClusterReport {
    let _lock = match action {
        Action::Status => None,
        Action::Down | Action::Kill if !layout.state_dir.exists() => None,
        Action::Init | Action::Up | Action::Down | Action::Kill | Action::Restart => {
            match Lock::take(layout) {
                Ok(lock) => Some(lock),
                Err(why) => return failed(layout.report(action), why),
            }
        }
    };
    match (action, node) {
        (Action::Kill, Some(node)) => kill(layout, node, Timing::LIVE),
        (Action::Restart, Some(node)) => with_server(root, action, layout, Some(node)),
        (Action::Kill | Action::Restart, None) => failed(
            layout.report(action),
            format!("`{}` needs --node", action.name()),
        ),
        (Action::Status, _) => status(layout),
        (Action::Down, _) => down(layout, Timing::LIVE),
        (Action::Init | Action::Up, _) => with_server(root, action, layout, None),
    }
}

/// Builds the server, then runs the action that starts or stamps with it.
fn with_server(root: &Path, action: Action, layout: &Layout, node: Option<u64>) -> ClusterReport {
    let server = match build_server(root) {
        Ok(server) => server,
        Err(why) => return failed(layout.report(action), why),
    };
    match (action, node) {
        (Action::Restart, Some(node)) => restart(layout, &server, Timing::LIVE, node),
        (Action::Init, _) => init(layout, &server),
        (Action::Up, _) => up(layout, &server, Timing::LIVE),
        (Action::Status | Action::Down | Action::Kill | Action::Restart, _) => failed(
            layout.report(action),
            format!("`{}` does not start a server", action.name()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_dir;

    const FAST: Timing = Timing {
        grace: Duration::from_millis(400),
        stop: Duration::from_secs(10),
    };

    #[test]
    fn lock_refuses_a_live_holder_and_replaces_a_stale_one() {
        let fixture = Fixture::new("lock");
        let layout = &fixture.layout;
        let held = Lock::take(layout).unwrap();
        let err = Lock::take(layout).err().expect("a live holder is refused");
        assert!(err.contains("holds"), "rule: one launcher at a time; {err}");
        drop(held);
        assert!(
            !layout.state_dir.join("lock.json").exists(),
            "rule: released on drop"
        );
        fs::write(
            layout.state_dir.join("lock.json"),
            r#"{"pid":4294967,"start_ticks":1}"#,
        )
        .unwrap();
        let again = Lock::take(layout).expect("rule: a dead holder's lock is stale");
        drop(again);
    }

    #[test]
    fn a_rebuilt_binary_does_not_make_a_node_foreign() {
        let me = identity(std::process::id()).unwrap();
        let receipt = Receipt {
            pid: std::process::id(),
            identity: me.clone(),
        };
        assert_eq!(owned(&receipt), Owned::Ours, "rule: own identity matches");
        assert!(
            !me.exe.ends_with(" (deleted)"),
            "rule: suffix is stripped; {}",
            me.exe
        );
    }

    /// A loopback port nothing listens on now.
    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A `keyval-server` stand-in. `init` writes `nodes/N/journal.log` and `instance`, and
    /// answers like the real one, including its "initialised already" refusal. `run` needs
    /// a journal and then sleeps (bounded). Markers in the config's directory steer it (`ignore-term-N` makes `run` of node N ignore SIGTERM):
    /// `fail-init-N` makes `init` of node N fail; `fail-run-N` makes `run` of node N exit; every `run` records its pid in `pid-N`.
    const FAKE: &str = r#"#!/bin/sh
cmd=$1; shift
while [ $# -gt 0 ]; do
  case $1 in
    --config) config=$2; shift 2;;
    --node) node=$2; shift 2;;
    --instance) instance=$2; shift 2;;
    *) shift;;
  esac
done
here=$(dirname "$config")
dir=$here/nodes/$node
case $cmd in
  init)
    if [ -e "$here/fail-init-$node" ]; then
      printf '{"schema":"keyval.server.error/v1","command":"init","code":"storage","message":"disk on fire","data_dir":null}\n'
      exit 3
    fi
    if [ -s "$dir/journal.log" ]; then
      printf '{"schema":"keyval.server.error/v1","command":"init","code":"already_initialised","message":"the data directory is initialised already (instance %s); re-initialising a directory that holds data is the owner'"'"'s decision, never init'"'"'s","data_dir":"%s"}\n' "$(cat "$dir/instance")" "$dir"
      exit 3
    fi
    mkdir -p "$dir"
    printf %s "$instance" > "$dir/instance"
    printf genesis > "$dir/journal.log"
    printf '{"schema":"keyval.server.init/v1","data_dir":"%s","directory":"created","replaced":null,"node":%s,"cluster_id":"t","instance_id":"%s","voters":[1,2,3,4,5],"journal_bytes":7}\n' "$dir" "$node" "$instance"
    ;;
  run)
    [ -s "$dir/journal.log" ] || exit 4
    echo $$ > "$here/pid-$node"
    [ -e "$here/fail-run-$node" ] && exit 5
    [ -e "$here/ignore-term-$node" ] && trap '' TERM
    i=0
    while [ $i -lt 120 ]; do sleep 1; i=$((i+1)); done
    ;;
esac
"#;

    struct Fixture {
        dir: PathBuf,
        layout: Layout,
        server: PathBuf,
        ports: Vec<u16>,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = scratch_dir(name);
            let server = dir.join("fake-server.sh");
            fs::write(&server, FAKE).unwrap();
            fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();
            let ports: Vec<u16> = (0..10).map(|_| free_port()).collect();
            let mut config = String::from("cluster_id = \"test\"\n");
            for id in 1..=5_usize {
                let _ = write!(
                    config,
                    "\n[[nodes]]\nid = {id}\nclient_address = \"127.0.0.1:{}\"\n\
                     peer_address = \"127.0.0.1:{}\"\ndata_dir = \"nodes/{id}\"\n",
                    ports[id - 1],
                    ports[id + 4]
                );
            }
            fs::write(dir.join("cluster.toml"), config).unwrap();
            let layout = Layout {
                config: dir.join("cluster.toml"),
                state_dir: dir.join("state"),
            };
            Self {
                dir,
                layout,
                server,
                ports,
            }
        }

        fn node_dir(&self, id: u64) -> PathBuf {
            self.dir.join(format!("nodes/{id}"))
        }

        fn init(&self) -> ClusterReport {
            init(&self.layout, &self.server)
        }

        fn up(&self) -> ClusterReport {
            up(&self.layout, &self.server, FAST)
        }

        /// The instance each node's directory was stamped with.
        fn stamped(&self) -> Vec<Option<String>> {
            (1..=5)
                .map(|id| fs::read_to_string(self.node_dir(id).join("instance")).ok())
                .collect()
        }

        /// Every file under `nodes/`, with its content: what "unmodified" compares.
        fn snapshot(&self) -> Vec<(PathBuf, String)> {
            fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
                let Ok(entries) = fs::read_dir(dir) else {
                    return;
                };
                for entry in entries {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else {
                        let text = fs::read_to_string(&path).unwrap_or_default();
                        out.push((path, text));
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.dir.join("nodes"), &mut out);
            out.sort();
            out
        }
    }

    impl Drop for Fixture {
        /// A failed test leaves no node behind.
        fn drop(&mut self) {
            let _ = down(&self.layout, FAST);
        }
    }

    fn states(report: &ClusterReport) -> Vec<NodeState> {
        report.nodes.iter().map(|node| node.state).collect()
    }

    #[test]
    fn the_config_resolves_data_dirs_against_its_own_directory() {
        let fixture = Fixture::new("cluster-config");
        let nodes = load_config(&fixture.layout.config).unwrap();
        assert_eq!(nodes.len(), 5, "rule: five nodes; nodes={nodes:?}");
        assert_eq!(
            nodes[2].data_dir,
            fixture.dir.join("nodes/3"),
            "rule: a relative data_dir resolves against the config's directory (§7.7 step 0)"
        );
        assert_eq!(nodes[0].client.port(), fixture.ports[0]);
        assert_eq!(nodes[0].peer.port(), fixture.ports[5]);
    }

    #[test]
    fn a_config_that_cannot_be_managed_is_refused_with_its_reason() {
        let dir = scratch_dir("cluster-bad-config");
        let check = |text: &str, expect: &str| {
            let path = dir.join("bad.toml");
            fs::write(&path, text).unwrap();
            let err = load_config(&path).unwrap_err();
            assert!(
                err.contains(expect),
                "rule: the refusal names the defect; config={text:?} expected={expect:?} err={err}"
            );
        };
        check("cluster_id = \"x\"\n", "no [[nodes]]");
        let node = "[[nodes]]\nid = 1\nclient_address = \"127.0.0.1:1\"\n\
                    peer_address = \"127.0.0.1:2\"\ndata_dir = \"d\"\n";
        check(&format!("{node}{node}"), "listed twice");
        check(&node.replace("127.0.0.1:1", "nope"), "client_address");
        check(&node.replace("data_dir = \"d\"\n", ""), "data_dir");
        assert!(
            load_config(&dir.join("missing.toml")).is_err(),
            "rule: a missing config is refused"
        );
    }

    #[test]
    fn instance_ids_are_32_hex_digits_and_differ_per_call() {
        let ids: Vec<String> = (0..4).map(|_| random_instance_id().unwrap()).collect();
        for id in &ids {
            assert!(
                id.len() == 32
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "rule: an instance ID is 32 lowercase hex digits; id={id}"
            );
        }
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "rule: IDs are random; ids={ids:?}");
    }

    #[test]
    fn the_default_state_dir_is_stable_and_differs_per_config() {
        let root = Path::new("/repo");
        let local = Layout::new(root, PathBuf::from("/repo/config/local.toml"), None);
        assert_eq!(
            local.state_dir,
            Layout::new(root, PathBuf::from("/repo/config/local.toml"), None).state_dir,
            "rule: the same config always finds its receipts"
        );
        let other = Layout::new(root, PathBuf::from("/run/7/local.toml"), None);
        assert_ne!(
            local.state_dir, other.state_dir,
            "rule: two configs with one file name never share receipts"
        );
        assert!(local.state_dir.starts_with("/repo/.local/cluster"));
        let given = Layout::new(
            root,
            PathBuf::from("/x.toml"),
            Some(PathBuf::from("/run/7/state")),
        );
        assert_eq!(given.state_dir, PathBuf::from("/run/7/state"));
    }

    #[test]
    fn a_fresh_init_stamps_one_instance_id_into_all_five_directories() {
        let fixture = Fixture::new("cluster-init-fresh");
        let report = fixture.init();
        assert!(report.ok, "rule: a fresh init succeeds; report={report:#?}");
        assert_eq!(states(&report), vec![NodeState::Stamped; 5]);
        let stamped = fixture.stamped();
        let first = stamped[0].clone().expect("node 1 is stamped");
        assert_eq!(
            first.len(),
            32,
            "rule: 128 bits as 32 hex digits; id={first}"
        );
        assert!(
            stamped
                .iter()
                .all(|id| id.as_deref() == Some(first.as_str())),
            "rule: one instance ID in all five directories; stamped={stamped:?}"
        );
        assert_eq!(report.instance_id.as_deref(), Some(first.as_str()));
        assert_eq!(
            InstanceRecord::read(&fixture.layout).unwrap().unwrap(),
            InstanceRecord {
                instance_id: first,
                nodes: vec![1, 2, 3, 4, 5],
                complete: true
            },
            "rule: the record says all five are stamped"
        );
    }

    #[test]
    fn an_empty_journal_left_by_an_interrupted_init_is_a_fresh_directory() {
        let fixture = Fixture::new("cluster-init-empty-journal");
        fs::create_dir_all(fixture.node_dir(2)).unwrap();
        fs::write(fixture.node_dir(2).join(JOURNAL), "").unwrap();
        fs::write(fixture.node_dir(2).join(LOCK), "").unwrap();
        let report = fixture.init();
        assert!(
            report.ok,
            "rule: a directory holding only an empty journal and lock is fresh; report={report:#?}"
        );
    }

    #[test]
    fn init_refuses_a_used_directory_and_modifies_nothing() {
        let fixture = Fixture::new("cluster-init-used");
        fs::create_dir_all(fixture.node_dir(3)).unwrap();
        fs::write(fixture.node_dir(3).join(JOURNAL), "somebody's data").unwrap();
        fs::create_dir_all(fixture.node_dir(5)).unwrap();
        fs::write(fixture.node_dir(5).join("stray"), "x").unwrap();
        let before = fixture.snapshot();
        let report = fixture.init();
        assert!(
            !report.ok,
            "rule: a used directory is refused; report={report:#?}"
        );
        let refused: Vec<u64> = report
            .nodes
            .iter()
            .filter(|node| node.state == NodeState::Refused)
            .map(|node| node.node)
            .collect();
        assert_eq!(refused, vec![3, 5], "rule: every used directory is named");
        assert_eq!(fixture.snapshot(), before, "rule: nothing was modified");
        assert!(
            !fixture.layout.state_dir.exists(),
            "rule: a refused init records no instance"
        );
    }

    #[test]
    fn init_never_repeats_over_a_completed_init() {
        let fixture = Fixture::new("cluster-init-twice");
        assert!(fixture.init().ok);
        let before = fixture.snapshot();
        let record = fs::read_to_string(fixture.layout.record()).unwrap();
        let again = fixture.init();
        assert!(
            !again.ok && again.error.as_deref().unwrap_or_default().contains("owner"),
            "rule: re-initialising is the owner's decision; report={again:#?}"
        );
        assert_eq!(fixture.snapshot(), before, "rule: the data is untouched");
        assert_eq!(fs::read_to_string(fixture.layout.record()).unwrap(), record);
    }

    #[test]
    fn an_interrupted_init_resumes_with_the_same_instance_id() {
        let fixture = Fixture::new("cluster-init-resume");
        fs::write(fixture.dir.join("fail-init-3"), "").unwrap();
        let broken = fixture.init();
        assert!(
            !broken.ok,
            "rule: the interruption is reported; report={broken:#?}"
        );
        assert_eq!(
            states(&broken),
            vec![
                NodeState::Stamped,
                NodeState::Stamped,
                NodeState::Refused,
                NodeState::Uninitialised,
                NodeState::Uninitialised
            ],
            "rule: init stops at the first refusal and says what it did not try"
        );
        let id = broken.instance_id.unwrap();
        fs::remove_file(fixture.dir.join("fail-init-3")).unwrap();
        let resumed = fixture.init();
        assert!(
            resumed.ok,
            "rule: a repeat completes the init; report={resumed:#?}"
        );
        assert_eq!(resumed.instance_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            fixture.stamped(),
            vec![Some(id); 5],
            "rule: the resumed init kept one instance ID across all five directories"
        );
    }

    #[test]
    fn a_resumed_init_refuses_a_directory_stamped_with_another_instance() {
        let fixture = Fixture::new("cluster-init-mismatch");
        fs::write(fixture.dir.join("fail-init-4"), "").unwrap();
        assert!(!fixture.init().ok);
        fs::remove_file(fixture.dir.join("fail-init-4")).unwrap();
        // Node 2 is rebound to a foreign cluster behind the launcher's back.
        let foreign = "f".repeat(32);
        fs::write(fixture.node_dir(2).join("instance"), &foreign).unwrap();
        let before = fixture.snapshot();
        let report = fixture.init();
        assert!(
            !report.ok,
            "rule: a mismatched directory is refused; report={report:#?}"
        );
        assert_eq!(report.nodes[1].state, NodeState::Refused);
        assert_eq!(
            fixture.snapshot(),
            before,
            "rule: the mismatched directory (and every other) is left as it was"
        );
        assert!(
            !InstanceRecord::read(&fixture.layout)
                .unwrap()
                .unwrap()
                .complete,
            "rule: the init is not recorded as complete"
        );
    }

    #[test]
    fn a_resumed_init_refuses_stray_content_before_stamping_anything() {
        let fixture = Fixture::new("cluster-init-resume-stray");
        fs::write(fixture.dir.join("fail-init-2"), "").unwrap();
        assert!(!fixture.init().ok);
        fs::remove_file(fixture.dir.join("fail-init-2")).unwrap();
        // Node 5 gains a stray file and no journal, behind the launcher's back.
        fs::create_dir_all(fixture.node_dir(5)).unwrap();
        fs::write(fixture.node_dir(5).join("stray"), "x").unwrap();
        let before = fixture.snapshot();
        let report = fixture.init();
        assert!(!report.ok, "rule: stray content is refused; {report:#?}");
        assert_eq!(
            fixture.snapshot(),
            before,
            "rule: nothing is stamped before the refusal (fresh nodes 2-4 stay fresh)"
        );
    }

    #[test]
    fn one_config_spelled_two_ways_shares_a_state_directory() {
        let fixture = Fixture::new("cluster-spelling");
        let direct = Layout::new(&fixture.dir, fixture.dir.join("cluster.toml"), None);
        let roundabout = Layout::new(
            &fixture.dir,
            fixture.dir.join("nodes/../cluster.toml"),
            None,
        );
        fs::create_dir_all(fixture.dir.join("nodes")).unwrap();
        let roundabout_again = Layout::new(
            &fixture.dir,
            fixture.dir.join("nodes/../cluster.toml"),
            None,
        );
        assert_eq!(
            direct.state_dir, roundabout_again.state_dir,
            "rule: `..` spellings of one config share receipts; {roundabout:?}"
        );
    }

    #[test]
    fn instance_ids_are_read_from_the_servers_refusal_only_when_well_formed() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            instance_in(&format!("initialised already (instance {id}); re-init")),
            Some(id)
        );
        assert_eq!(instance_in("(instance 0123)"), None);
        assert_eq!(instance_in(&format!("(instance {})", "G".repeat(32))), None);
        assert_eq!(instance_in("no instance here"), None);
    }

    #[test]
    fn up_refuses_uninitialised_directories_and_starts_nothing() {
        let fixture = Fixture::new("cluster-up-uninitialised");
        let report = fixture.up();
        assert!(
            !report.ok,
            "rule: uninitialised directories are refused; report={report:#?}"
        );
        assert_eq!(states(&report), vec![NodeState::Uninitialised; 5]);
        assert!(
            !fixture.layout.state_dir.exists(),
            "rule: a refused up writes no receipt and no log"
        );
    }

    #[test]
    fn up_refuses_after_an_interrupted_init() {
        let fixture = Fixture::new("cluster-up-after-interrupted-init");
        fs::write(fixture.dir.join("fail-init-5"), "").unwrap();
        assert!(!fixture.init().ok);
        let report = fixture.up();
        assert!(
            !report.ok
                && report
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("interrupted"),
            "rule: up will not start a half-stamped cluster; report={report:#?}"
        );
    }

    #[test]
    fn up_refuses_an_occupied_port_and_starts_nothing() {
        let fixture = Fixture::new("cluster-up-port");
        assert!(fixture.init().ok);
        let _held = TcpListener::bind(("127.0.0.1", fixture.ports[7])).unwrap();
        let report = fixture.up();
        assert!(
            !report.ok,
            "rule: an occupied port is refused; report={report:#?}"
        );
        let refused: Vec<u64> = report
            .nodes
            .iter()
            .filter(|node| node.state == NodeState::Refused)
            .map(|node| node.node)
            .collect();
        assert_eq!(
            refused,
            vec![3],
            "rule: node 3's peer port is the occupied one"
        );
        assert!(
            !fixture.layout.receipt(1).exists(),
            "rule: no node started, so none has a receipt"
        );
    }

    #[test]
    fn up_status_and_down_manage_five_nodes_by_receipt_and_keep_the_data() {
        let fixture = Fixture::new("cluster-lifecycle");
        assert!(fixture.init().ok);
        let data = fixture.snapshot();
        let up = fixture.up();
        assert!(up.ok, "rule: five initialised nodes start; report={up:#?}");
        assert_eq!(states(&up), vec![NodeState::Started; 5]);
        let status = status(&fixture.layout);
        assert!(status.ok);
        assert_eq!(states(&status), vec![NodeState::Running; 5]);
        assert_eq!(
            status.nodes.iter().map(|n| n.pid).collect::<Vec<_>>(),
            up.nodes.iter().map(|n| n.pid).collect::<Vec<_>>(),
            "rule: status names the PIDs up started"
        );
        for pid in up.nodes.iter().filter_map(|node| node.pid) {
            assert!(identity(pid).is_some(), "rule: pid {pid} runs");
        }
        let again = fixture.up();
        assert!(
            again.ok,
            "rule: a second up is not an error; report={again:#?}"
        );
        assert_eq!(
            again.nodes.iter().map(|n| n.pid).collect::<Vec<_>>(),
            up.nodes.iter().map(|n| n.pid).collect::<Vec<_>>(),
            "rule: a running node is not started twice"
        );
        assert_eq!(states(&again), vec![NodeState::Running; 5]);
        let stopped = down(&fixture.layout, FAST);
        assert!(
            stopped.ok,
            "rule: down stops what up started; report={stopped:#?}"
        );
        assert_eq!(states(&stopped), vec![NodeState::Stopped; 5]);
        for pid in up.nodes.iter().filter_map(|node| node.pid) {
            assert!(
                identity(pid).is_none(),
                "rule: pid {pid} is gone after down"
            );
        }
        for id in 1..=5 {
            assert!(
                !fixture.layout.receipt(id).exists(),
                "rule: down removes the receipt of node {id}"
            );
        }
        assert_eq!(
            fixture.snapshot(),
            data,
            "rule: the cluster's lifecycle never touches data"
        );
        assert_eq!(
            states(&super::status(&fixture.layout)),
            vec![NodeState::Initialised; 5]
        );
    }

    #[test]
    fn a_node_that_exits_during_start_up_rolls_back_the_nodes_started_with_it() {
        let fixture = Fixture::new("cluster-up-partial");
        assert!(fixture.init().ok);
        fs::write(fixture.dir.join("fail-run-4"), "").unwrap();
        let report = fixture.up();
        assert!(
            !report.ok,
            "rule: a node that dies fails the up; report={report:#?}"
        );
        assert!(
            report
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("node 4"),
            "rule: the failing node is named; report={report:#?}"
        );
        for id in 1..=5 {
            assert!(
                !fixture.layout.receipt(id).exists(),
                "rule: a partial start leaves no receipt; node={id}"
            );
        }
        for entry in &report.nodes {
            assert!(
                entry.state != NodeState::Started && entry.state != NodeState::Running,
                "rule: no node is reported running after a rollback; report={report:#?}"
            );
        }
        let status = status(&fixture.layout);
        assert_eq!(states(&status), vec![NodeState::Initialised; 5]);
        let started: Vec<(u64, u32)> = (1..=5)
            .filter_map(|id| {
                let text = fs::read_to_string(fixture.dir.join(format!("pid-{id}"))).ok()?;
                Some((id, text.trim().parse().ok()?))
            })
            .collect();
        assert!(
            started.len() >= 2,
            "rule: the nodes started before node 4 failed left their pids; started={started:?}"
        );
        for (id, pid) in started {
            assert!(
                identity(pid).is_none(),
                "rule: rollback killed node {id}'s process (pid {pid}); report={report:#?}"
            );
        }
    }

    /// A receipt for a process that is not the one recorded: PID reuse.
    fn forge_receipt(fixture: &Fixture, node: u64, pid: u32) {
        fs::create_dir_all(&fixture.layout.state_dir).unwrap();
        let mut identity = identity(pid).expect("the pid runs");
        identity.start_ticks += 1;
        let nodes = load_config(&fixture.layout.config).unwrap();
        let receipt = Receipt { pid, identity };
        fs::write(
            fixture.layout.receipt(node),
            receipt
                .to_json(&nodes[0], &fixture.layout.config)
                .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_recycled_pid_is_never_signalled_and_its_receipt_is_cleared() {
        let fixture = Fixture::new("cluster-pid-reuse");
        assert!(fixture.init().ok);
        // A live process this launcher did not start: this very test process.
        let foreign = std::process::id();
        forge_receipt(&fixture, 2, foreign);
        let seen = status(&fixture.layout);
        assert_eq!(seen.nodes[1].state, NodeState::Stale, "report={seen:#?}");
        assert!(seen.ok, "rule: a stale receipt is reported, not an error");
        assert!(
            fixture.layout.receipt(2).exists(),
            "rule: status changes nothing"
        );
        let stopped = down(&fixture.layout, FAST);
        assert!(
            stopped.ok,
            "rule: down clears a stale receipt; report={stopped:#?}"
        );
        assert_eq!(stopped.nodes[1].state, NodeState::Stale);
        assert!(!fixture.layout.receipt(2).exists());
        assert!(
            identity(foreign).is_some(),
            "rule: the foreign process (pid {foreign}) was not signalled"
        );
        forge_receipt(&fixture, 2, foreign);
        let started = fixture.up();
        assert!(
            started.ok,
            "rule: up replaces a stale receipt; report={started:#?}"
        );
        assert_eq!(started.nodes[1].state, NodeState::Started);
        assert_ne!(started.nodes[1].pid, Some(foreign));
    }

    fn pids(report: &ClusterReport) -> Vec<Option<u32>> {
        report.nodes.iter().map(|node| node.pid).collect()
    }

    #[test]
    fn kill_sigkills_only_the_named_node_and_touches_no_data() {
        let fixture = Fixture::new("cluster-kill");
        assert!(fixture.init().ok);
        let up = fixture.up();
        assert!(up.ok, "report={up:#?}");
        let victim = up.nodes[2].pid.expect("node 3 runs");
        let data = fixture.snapshot();
        let report = kill(&fixture.layout, 3, FAST);
        assert!(report.ok, "rule: a receipted node is killed; {report:#?}");
        assert_eq!(report.action, Action::Kill);
        assert_eq!(
            report.nodes.len(),
            1,
            "rule: one node is named; {report:#?}"
        );
        assert_eq!(report.nodes[0].state, NodeState::Killed);
        assert!(identity(victim).is_none(), "rule: pid {victim} is gone");
        assert!(
            !fixture.layout.receipt(3).exists(),
            "rule: its receipt is cleared"
        );
        assert_eq!(fixture.snapshot(), data, "rule: no data directory changes");
        let seen = status(&fixture.layout);
        let states: Vec<NodeState> = seen.nodes.iter().map(|n| n.state).collect();
        assert_eq!(
            states,
            [
                NodeState::Running,
                NodeState::Running,
                NodeState::Initialised,
                NodeState::Running,
                NodeState::Running
            ],
            "rule: only node 3 stopped; {seen:#?}"
        );
    }

    #[test]
    fn kill_of_a_node_that_is_not_running_or_not_in_the_config_fails_quietly() {
        let fixture = Fixture::new("cluster-kill-absent");
        assert!(fixture.init().ok);
        let never = kill(&fixture.layout, 2, FAST);
        assert!(!never.ok, "rule: nothing to kill is an error; {never:#?}");
        assert_eq!(never.nodes[0].state, NodeState::Initialised);
        let unknown = kill(&fixture.layout, 9, FAST);
        assert!(!unknown.ok && unknown.nodes.is_empty(), "{unknown:#?}");
        assert!(
            unknown.error.as_deref().unwrap_or("").contains("node 9"),
            "rule: the error names the node; {unknown:#?}"
        );
    }

    #[test]
    fn kill_refuses_a_foreign_process_at_the_pid() {
        let fixture = Fixture::new("cluster-kill-foreign");
        assert!(fixture.init().ok);
        let foreign = std::process::id();
        forge_receipt(&fixture, 2, foreign);
        let report = kill(&fixture.layout, 2, FAST);
        assert!(!report.ok, "rule: a foreign pid is refused; {report:#?}");
        assert_eq!(report.nodes[0].state, NodeState::Refused);
        assert!(
            identity(foreign).is_some(),
            "rule: the foreign process (pid {foreign}) was not signalled"
        );
        assert!(
            !fixture.layout.receipt(2).exists(),
            "rule: the dead receipt goes"
        );
    }

    #[test]
    fn restart_replaces_the_process_and_keeps_the_others() {
        let fixture = Fixture::new("cluster-restart");
        assert!(fixture.init().ok);
        let up = fixture.up();
        assert!(up.ok, "report={up:#?}");
        let data = fixture.snapshot();
        let report = restart(&fixture.layout, &fixture.server, FAST, 2);
        assert!(report.ok, "rule: a running node restarts; {report:#?}");
        assert_eq!(report.action, Action::Restart);
        assert_eq!(report.nodes[0].state, NodeState::Started);
        let fresh = report.nodes[0].pid.expect("a new pid");
        assert_ne!(Some(fresh), up.nodes[1].pid, "rule: a new process");
        assert!(
            identity(up.nodes[1].pid.unwrap()).is_none(),
            "rule: the old one is dead"
        );
        let seen = status(&fixture.layout);
        assert_eq!(seen.nodes[1].pid, Some(fresh));
        for index in [0, 2, 3, 4] {
            assert_eq!(
                seen.nodes[index].pid,
                up.nodes[index].pid,
                "rule: node {} is untouched",
                index + 1
            );
        }
        assert_eq!(fixture.snapshot(), data, "rule: no data directory changes");
        assert!(
            pids(&seen).iter().all(Option::is_some),
            "rule: all five run; {seen:#?}"
        );
    }

    #[test]
    fn restart_that_cannot_start_leaves_the_running_node_alone() {
        let fixture = Fixture::new("cluster-restart-preflight");
        assert!(fixture.init().ok);
        let up = fixture.up();
        assert!(up.ok, "report={up:#?}");
        let pid = up.nodes[1].pid.expect("node 2 runs");
        fs::write(fixture.node_dir(2).join("journal.log"), "").unwrap();
        let report = restart(&fixture.layout, &fixture.server, FAST, 2);
        assert!(
            !report.ok,
            "rule: an unstartable node is refused; {report:#?}"
        );
        assert_eq!(report.nodes[0].state, NodeState::Refused);
        assert!(
            identity(pid).is_some(),
            "rule: the healthy node (pid {pid}) was not killed by a refused restart"
        );
        assert!(
            fixture.layout.receipt(2).exists(),
            "rule: its receipt stays"
        );
    }

    #[test]
    fn a_restart_that_fails_after_the_kill_says_the_node_was_killed() {
        let fixture = Fixture::new("cluster-restart-after-kill");
        assert!(fixture.init().ok);
        let up = fixture.up();
        assert!(up.ok, "report={up:#?}");
        let pid = up.nodes[1].pid.expect("node 2 runs");
        // Passes the preflight (initialised, journal present), then exits once started.
        fs::write(fixture.dir.join("fail-run-2"), "").unwrap();
        let report = restart(&fixture.layout, &fixture.server, FAST, 2);
        assert!(!report.ok, "rule: the new process dies; {report:#?}");
        assert!(
            identity(pid).is_none(),
            "rule: the old process (pid {pid}) was killed"
        );
        let detail = report.nodes[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains(&format!("pid {pid}")) && detail.contains("killed"),
            "rule: a failure after the kill reports the node as killed; detail={detail:?}, report={report:#?}"
        );
        assert!(
            !fixture.layout.receipt(2).exists(),
            "rule: no receipt is left for a node that is not running"
        );
    }

    #[test]
    fn kill_is_sigkill_and_not_a_polite_request() {
        assert_eq!(Signal::Kill.flag(), "-KILL");
        assert_eq!(Signal::Term.flag(), "-TERM");
        let fixture = Fixture::new("cluster-kill-vs-term");
        assert!(fixture.init().ok);
        fs::write(fixture.dir.join("ignore-term-2"), "").unwrap();
        let up = fixture.up();
        assert!(up.ok, "report={up:#?}");
        let pid = up.nodes[1].pid.expect("node 2 runs");
        let patient = Timing {
            grace: FAST.grace,
            stop: Duration::from_millis(500),
        };
        let stopped = down(&fixture.layout, patient);
        assert!(
            !stopped.ok,
            "rule: SIGTERM cannot stop node 2; {stopped:#?}"
        );
        assert_eq!(stopped.nodes[1].state, NodeState::Running);
        assert!(identity(pid).is_some(), "rule: node 2 shrugged off SIGTERM");
        let report = kill(&fixture.layout, 2, FAST);
        assert!(
            report.ok,
            "rule: SIGKILL stops what ignores SIGTERM; {report:#?}"
        );
        assert!(identity(pid).is_none(), "rule: pid {pid} is gone");
    }

    #[test]
    fn kill_clears_the_receipt_of_a_process_that_already_exited() {
        let fixture = Fixture::new("cluster-kill-gone");
        assert!(fixture.init().ok);
        let mut brief = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = brief.id();
        fs::create_dir_all(&fixture.layout.state_dir).unwrap();
        let nodes = load_config(&fixture.layout.config).unwrap();
        let receipt = Receipt {
            pid,
            identity: identity(pid).unwrap(),
        };
        fs::write(
            fixture.layout.receipt(2),
            receipt
                .to_json(&nodes[1], &fixture.layout.config)
                .to_string(),
        )
        .unwrap();
        brief.kill().unwrap();
        brief.wait().unwrap();
        let report = kill(&fixture.layout, 2, FAST);
        assert!(!report.ok, "rule: nothing was running; {report:#?}");
        assert_eq!(report.nodes[0].state, NodeState::Stale);
        assert!(
            !fixture.layout.receipt(2).exists(),
            "rule: the dead receipt goes"
        );
    }

    #[test]
    fn restart_over_a_foreign_pid_signals_nothing_and_starts_the_node() {
        let fixture = Fixture::new("cluster-restart-foreign");
        assert!(fixture.init().ok);
        let foreign = std::process::id();
        forge_receipt(&fixture, 2, foreign);
        let report = restart(&fixture.layout, &fixture.server, FAST, 2);
        assert!(
            report.ok,
            "rule: the node is not running, so it starts; {report:#?}"
        );
        assert_eq!(report.nodes[0].state, NodeState::Started);
        assert!(
            report.nodes[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("nothing signalled"),
            "rule: the report says nothing was signalled; {report:#?}"
        );
        assert!(
            identity(foreign).is_some(),
            "rule: pid {foreign} was not signalled"
        );
    }

    #[test]
    fn restart_starts_a_node_that_is_not_running() {
        let fixture = Fixture::new("cluster-restart-cold");
        assert!(fixture.init().ok);
        let report = restart(&fixture.layout, &fixture.server, FAST, 4);
        assert!(
            report.ok,
            "rule: restart starts a stopped node; {report:#?}"
        );
        assert_eq!(report.nodes[0].state, NodeState::Started);
    }

    #[test]
    fn restart_refuses_an_occupied_port_and_an_uninitialised_directory() {
        let fixture = Fixture::new("cluster-restart-refused");
        assert!(fixture.init().ok);
        assert!(fixture.up().ok);
        assert!(kill(&fixture.layout, 2, FAST).ok);
        let squatter = TcpListener::bind(("127.0.0.1", fixture.ports[1])).unwrap();
        let data = fixture.snapshot();
        let blocked = restart(&fixture.layout, &fixture.server, FAST, 2);
        assert!(
            !blocked.ok,
            "rule: an occupied port is refused; {blocked:#?}"
        );
        assert_eq!(blocked.nodes[0].state, NodeState::Refused);
        let why = blocked.nodes[0].detail.as_deref().unwrap_or_default();
        assert!(
            why.contains(&fixture.ports[1].to_string()) && why.contains("in use"),
            "rule: the refusal names the occupied port {}; detail={why:?}",
            fixture.ports[1]
        );
        assert!(
            !fixture.layout.receipt(2).exists(),
            "rule: nothing was started"
        );
        assert_eq!(fixture.snapshot(), data);
        drop(squatter);
        // Node 5 is up; its journal is emptied behind the launcher's back, so it holds no
        // genesis any more: restart refuses to start it over nothing.
        assert!(kill(&fixture.layout, 5, FAST).ok);
        fs::write(fixture.node_dir(5).join("journal.log"), "").unwrap();
        let bare = restart(&fixture.layout, &fixture.server, FAST, 5);
        assert!(!bare.ok, "rule: no genesis, no start; {bare:#?}");
        assert_eq!(bare.nodes[0].state, NodeState::Refused);
    }

    #[test]
    fn a_receipt_that_does_not_parse_is_reported_and_left_alone() {
        let fixture = Fixture::new("cluster-bad-receipt");
        assert!(fixture.init().ok);
        fs::create_dir_all(&fixture.layout.state_dir).unwrap();
        fs::write(fixture.layout.receipt(1), "not json").unwrap();
        for report in [
            status(&fixture.layout),
            down(&fixture.layout, FAST),
            fixture.up(),
        ] {
            assert!(
                !report.ok,
                "rule: a damaged receipt fails the command; report={report:#?}"
            );
            assert_eq!(report.nodes[0].state, NodeState::Refused);
            assert_eq!(
                fs::read_to_string(fixture.layout.receipt(1)).unwrap(),
                "not json",
                "rule: the damaged receipt is not rewritten or deleted"
            );
        }
    }

    #[test]
    fn the_report_names_every_node_and_says_why_it_failed() {
        let fixture = Fixture::new("cluster-render");
        let report = fixture.up();
        let text = report.render_human();
        assert!(
            text.contains("FAILED") && text.contains("error:"),
            "text={text}"
        );
        assert_eq!(text.matches("node ").count(), 5, "text={text}");
        let ok = fixture.init().render_human();
        assert!(
            ok.contains("cluster init: ok") && ok.contains("instance "),
            "text={ok}"
        );
    }
}
