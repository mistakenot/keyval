//! `./dev demo m1 [--assert]`: the M1 owner demo (PLAN.md §11.6, §12.2 W19), the owner's
//! first look at running output. It orchestrates what exists and adds nothing new:
//!
//! 1. builds `keyval-server` and `keyval-sim`, and reports the build ID (the gate's own
//!    `sha256:` over the tree and its build inputs) and whether the working tree is dirty;
//! 2. runs **one real node** from a 1-voter config (D-45) in a fresh run directory under
//!    `.local/demo/m1/`, kept afterwards (rule 18: no node's data is ever deleted):
//!    `keyval-server init`, then `keyval-server run` as a child process with a PID receipt;
//!    a PUT, a GET, a SIGKILL of exactly the PID the receipt names, a restart from the same
//!    data directory, and a GET of the same value once the restarted node has applied the
//!    PUT's index;
//! 3. runs `./dev sim run --seed 42 --profile slice` twice, each a fresh process, and checks
//!    that the trace hashes are identical;
//! 4. prints what is implemented and what is pending (P6), computed from the work graph and
//!    the committed tracker export rather than written by hand, with the exact commands, the
//!    build ID and the visible limitations (§11.6).
//!
//! Every step is a `keyval.dev.step/v1` record, to stderr and to the run directory's
//! `steps.jsonl`; the first failed step ends the demo, which exits 1 (W01.6). Without
//! `--assert` the demo pauses between its parts when stdin is a terminal; with it, it never
//! pauses.
//!
//! **What is gated (br-fhk3, M1 audit F-01).** Check DEMO-M1 gates the demo's logic only:
//! its tests never call [`run`]. The end-to-end run (`./dev demo m1 --assert`) is a manual
//! run, not a gated check: it needs git over the live checkout and a full build of the
//! server and the simulator, which the git-isolated gate candidate cannot give it. Its
//! evidence is the run directory under `.local/demo/m1/`, test evidence and not proof.
//!
//! **The node's process.** The receipt (`node-1.pid.json`) exists exactly while its PID is a
//! child this module has not reaped, so the kernel cannot have recycled it: a kill signals
//! only that PID, then reaps it and removes the receipt (W18.4's discipline, rule 18). Every
//! answer is checked against `/v1/cluster`'s `cluster_id`, unique to the run, so the demo
//! never trusts a stranger holding its port. A demo that stops early still stops its node.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::clock::Utc;
use crate::output::Report;
use crate::process;
use crate::steplog::{Step, StepLog, StepRecord};

/// Schema identifier of the demo's report (`schemas/dev/demo.v1.schema.json`).
pub const SCHEMA: &str = "keyval.dev.demo/v1";

/// The step log's flow name.
pub const FLOW: &str = "demo m1";

/// Where the runs go, relative to the workspace root.
pub const RUN_ROOT: &str = ".local/demo/m1";

/// What the demo says it is, before anything else (P6).
pub const LABEL: &str = "M1 owner demo: one real keyval node on this machine (a 1-voter cluster) \
                         and the five-node simulated slice; not a multi-node cluster";

/// The key and value the demo writes.
const KEY: &str = "demo/m1";

/// The longest any one wait lasts: a node to lead, an index to apply.
const WAIT: Duration = Duration::from_secs(30);

/// The longest a build or a simulated run lasts.
const LIMIT: Duration = Duration::from_mins(20);

/// The one-voter fixture the demo's config starts from (D-45).
const ONE_VOTER: &str = "config/fixtures/one-voter.toml";

/// What the demo admits it cannot show yet (§11.6 "visible limitations").
pub const LIMITATIONS: &[&str] = &[
    "one real node only: a real multi-node cluster needs peer transport (W21) and `./dev \
     cluster` (W22), M2",
    "the five-node run is the deterministic simulator (`keyval-sim`), not five processes",
    "the HTTP API serves PUT and GET of a key, health and /v1/cluster; DELETE, CAS, SCAN and \
     the status lookup over HTTP are pending",
    "no TLS and no authentication: the node listens on loopback only",
    "the build ID covers the committed tree; a dirty working tree is reported, not hashed",
];

/// One work item and whether its bead is closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ItemStatus {
    /// The §12.2 ID, e.g. `W18`.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Its milestone, e.g. `m1`.
    pub milestone: String,
    /// Whether its bead is closed in the committed tracker export.
    pub implemented: bool,
}

/// The build the demo ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Build {
    /// The gate's build ID over the committed tree.
    pub id: String,
    /// `HEAD^{tree}`.
    pub tree: String,
    /// Whether tracked files differ from that tree.
    pub dirty: bool,
}

/// The two simulated runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SimPair {
    /// The first run's trace hash.
    pub first: String,
    /// The second run's trace hash.
    pub second: String,
}

/// `keyval.dev.demo/v1`: the demo's report.
#[derive(Clone, Debug, Serialize)]
pub struct DemoReport {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// Whether every step passed.
    pub ok: bool,
    /// [`LABEL`].
    pub label: &'static str,
    /// Always `m1`.
    pub milestone: &'static str,
    /// Whether it ran non-interactively, asserting every step.
    pub assert: bool,
    /// The build, if it was identified.
    pub build: Option<Build>,
    /// The run directory, kept afterwards.
    pub run_dir: String,
    /// Every step, in order; the last one failed if the demo did not pass.
    pub steps: Vec<StepRecord>,
    /// The two simulated runs' hashes, if both ran.
    pub sim: Option<SimPair>,
    /// The work items by milestone, implemented or pending.
    pub items: Vec<ItemStatus>,
    /// The commands that repeat each part by hand.
    pub commands: Vec<String>,
    /// [`LIMITATIONS`].
    pub limitations: Vec<&'static str>,
    /// The step log file.
    pub step_log: String,
}

impl Report for DemoReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut text = format!("{}\n", self.label);
        match &self.build {
            Some(build) => {
                let _ = writeln!(
                    text,
                    "build {} (tree {}{})",
                    build.id,
                    build.tree,
                    if build.dirty {
                        "; the working tree is DIRTY, so this is not exactly that build"
                    } else {
                        ""
                    }
                );
            }
            None => text.push_str("build: not identified\n"),
        }
        text.push_str("\nsteps:\n");
        for step in &self.steps {
            let mark = match step.outcome {
                crate::steplog::Outcome::Passed => "ok  ",
                crate::steplog::Outcome::Failed => "FAIL",
            };
            let _ = writeln!(text, "  {mark} {}: {}", step.step, step.observed);
            if let Some(rerun) = &step.rerun {
                let _ = writeln!(text, "       re-run: {rerun}");
            }
        }
        if let Some(sim) = &self.sim {
            let _ = writeln!(
                text,
                "\nsimulated slice, seed 42, two fresh processes: trace hashes {} and {}{}",
                sim.first,
                sim.second,
                if sim.first == sim.second {
                    " (identical)"
                } else {
                    " (DIFFERENT)"
                }
            );
        }
        for milestone in milestones(&self.items) {
            let (done, open): (Vec<&ItemStatus>, Vec<&ItemStatus>) = self
                .items
                .iter()
                .filter(|item| item.milestone == milestone)
                .partition(|item| item.implemented);
            let _ = writeln!(
                text,
                "\n{}: {} of {} work items implemented",
                milestone.to_uppercase(),
                done.len(),
                done.len() + open.len()
            );
            if milestone == "m1" {
                for item in &done {
                    let _ = writeln!(text, "  implemented  {} {}", item.id, item.title);
                }
            }
            for item in open
                .iter()
                .take(if milestone == "m1" { usize::MAX } else { 5 })
            {
                let _ = writeln!(text, "  pending      {} {}", item.id, item.title);
            }
            if milestone != "m1" && open.len() > 5 {
                let _ = writeln!(text, "  … and {} more pending", open.len() - 5);
            }
        }
        text.push_str("\nvisible limitations:\n");
        for limitation in &self.limitations {
            let _ = writeln!(text, "  - {limitation}");
        }
        text.push_str("\nto repeat by hand:\n");
        for command in &self.commands {
            let _ = writeln!(text, "  {command}");
        }
        let _ = write!(
            text,
            "\nrun directory {} (kept); step log {}\nM1 demo: {}",
            self.run_dir,
            self.step_log,
            if self.ok { "PASSED" } else { "FAILED" }
        );
        text
    }
}

/// The milestones `items` name, in order.
fn milestones(items: &[ItemStatus]) -> Vec<String> {
    items
        .iter()
        .map(|item| item.milestone.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// How the demo runs.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Never pause (the end-to-end run is manual; see the module docs).
    pub assert: bool,
    /// Whether its progress also goes to stdout as it happens (human mode).
    pub progress: bool,
}

/// Runs the M1 demo from the workspace at `root`.
#[must_use]
pub fn run(root: &Path, options: Options) -> DemoReport {
    let run_name = format!("{}-{}", Utc::now().compact(), std::process::id());
    let run_dir = root.join(RUN_ROOT).join(&run_name);
    let step_log_path = run_dir.join("steps.jsonl");
    let mut demo = Demo {
        root: root.to_path_buf(),
        options,
        run_dir: run_dir.clone(),
        run_name,
        log: StepLog::new(FLOW, Some(&step_log_path), true)
            .or_else(|_| StepLog::new(FLOW, None, true))
            .ok(),
        steps: Vec::new(),
        build: None,
        sim: None,
        node: None,
        commands: Vec::new(),
        items: Vec::new(),
    };
    let ok = demo.all().is_ok();
    demo.stop_node();
    DemoReport {
        schema: SCHEMA,
        ok,
        label: LABEL,
        milestone: "m1",
        assert: options.assert,
        build: demo.build,
        run_dir: run_dir.display().to_string(),
        steps: demo.steps,
        sim: demo.sim,
        items: demo.items,
        commands: demo.commands,
        limitations: LIMITATIONS.to_vec(),
        step_log: step_log_path.display().to_string(),
    }
}

/// A step that failed: the demo stops.
struct Stopped;

/// The demo's state while it runs.
struct Demo {
    root: PathBuf,
    options: Options,
    run_dir: PathBuf,
    run_name: String,
    log: Option<StepLog>,
    steps: Vec<StepRecord>,
    build: Option<Build>,
    sim: Option<SimPair>,
    node: Option<Node>,
    commands: Vec<String>,
    items: Vec<ItemStatus>,
}

/// What the built binaries are.
struct Binaries {
    server: PathBuf,
}

impl Demo {
    fn all(&mut self) -> Result<(), Stopped> {
        let rerun = "./dev demo m1 --assert";
        let binaries = self.step(
            "build",
            "cargo build -p server -p sim (local)",
            "both binaries build",
            rerun,
            |demo| demo.build_binaries(),
        )?;
        self.step(
            "build-id",
            "the gate's build ID over HEAD^{tree}",
            "a build ID",
            rerun,
            Self::identify,
        )?;
        self.step(
            "status",
            "registry/workgraph.toml + .beads/issues.jsonl",
            "every work item's status",
            rerun,
            Self::status,
        )?;
        self.node_part(&binaries, rerun)?;
        self.sim_part()
    }

    /// One real node: PUT, GET, kill, restart, GET.
    fn node_part(&mut self, binaries: &Binaries, rerun: &str) -> Result<(), Stopped> {
        self.pause("start one real node");
        let address = self.step(
            "init",
            "keyval-server init (one-voter config)",
            "an initialised data directory",
            rerun,
            |demo| demo.init(binaries),
        )?;
        self.step(
            "start",
            "keyval-server run, with a PID receipt",
            "the node is ready and leads",
            rerun,
            |demo| demo.start(binaries, address),
        )?;
        let index = self.step(
            "put",
            "PUT /v1/kv?key=demo/m1",
            "200, a first execution",
            rerun,
            |demo| demo.put(address),
        )?;
        let value = self.value();
        self.step(
            "get",
            "GET /v1/kv?key=demo/m1",
            "200 with the value written",
            rerun,
            |demo| demo.get(address, &value),
        )?;
        self.pause("SIGKILL the node and restart it");
        self.step(
            "kill",
            "SIGKILL the PID the receipt names",
            "the node dies of SIGKILL",
            rerun,
            Self::kill,
        )?;
        self.step(
            "restart",
            "keyval-server run again, same data directory",
            "the node is ready and leads again",
            rerun,
            |demo| demo.start(binaries, address),
        )?;
        self.step(
            "applied",
            "GET /v1/cluster until applied_index reaches the PUT's",
            "the PUT's index applied",
            rerun,
            |demo| demo.wait_applied(address, index),
        )?;
        self.step(
            "get-after-restart",
            "GET /v1/kv?key=demo/m1",
            "200 with the same value",
            rerun,
            |demo| demo.get(address, &value),
        )?;
        self.step(
            "stop",
            "SIGKILL the PID the receipt names",
            "the node stopped; its data directory kept",
            rerun,
            Self::kill,
        )?;
        Ok(())
    }

    /// The simulated slice twice, in fresh processes.
    fn sim_part(&mut self) -> Result<(), Stopped> {
        self.pause("run the simulated slice twice");
        let sim = "./dev sim run --seed 42 --profile slice";
        let first = self.step("sim-run-1", sim, "a passing simulated slice", sim, |demo| {
            demo.sim_run()
        })?;
        let second = self.step("sim-run-2", sim, "a passing simulated slice", sim, |demo| {
            demo.sim_run()
        })?;
        self.sim = Some(SimPair {
            first: first.clone(),
            second: second.clone(),
        });
        self.step(
            "sim-identical",
            "compare the two trace hashes",
            "identical trace hashes",
            sim,
            |_| {
                if first == second {
                    Ok((format!("both runs hash {first}"), ()))
                } else {
                    Err(format!("{first} and {second} differ"))
                }
            },
        )?;
        Ok(())
    }

    /// Runs one step, records it, and prints its line in human mode.
    fn step<T>(
        &mut self,
        name: &str,
        action: &str,
        expected: &str,
        rerun: &str,
        body: impl FnOnce(&mut Self) -> Result<(String, T), String>,
    ) -> Result<T, Stopped> {
        let started = Instant::now();
        let outcome = body(self);
        let (observed, value) = match outcome {
            Ok((observed, value)) => (Ok(observed), Some(value)),
            Err(why) => (Err(why), None),
        };
        let step = Step {
            name,
            action,
            expected,
            rerun,
        };
        let record = match self.log.as_mut() {
            Some(log) => log
                .record(step, observed, started.elapsed())
                .unwrap_or_else(|failed| *failed.record),
            None => return Err(Stopped),
        };
        if self.options.progress {
            let mark = match record.outcome {
                crate::steplog::Outcome::Passed => "ok  ",
                crate::steplog::Outcome::Failed => "FAIL",
            };
            let _ = writeln!(io::stdout().lock(), "{mark} {name}: {}", record.observed);
        }
        self.steps.push(record);
        value.ok_or(Stopped)
    }

    /// Waits for Enter before `next`, if a person is watching (not with `--assert`).
    fn pause(&self, next: &str) {
        if self.options.assert || !self.options.progress || !io::stdin().is_terminal() {
            return;
        }
        let _ = write!(io::stdout().lock(), "\npress Enter to {next} … ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let _ = io::stdin().lock().read_line(&mut line);
    }

    /// The value the demo writes: unique to the run.
    fn value(&self) -> String {
        format!("written by the M1 demo run {}", self.run_name)
    }

    fn build_binaries(&self) -> Result<(String, Binaries), String> {
        let mut cmd = Command::new("cargo");
        cmd.current_dir(&self.root).args([
            "build",
            "--locked",
            "--quiet",
            "--message-format=json",
            "-p",
            "server",
            "--bin",
            "keyval-server",
            "-p",
            "sim",
            "--bin",
            "keyval-sim",
        ]);
        let captured =
            process::run(&mut cmd, LIMIT).map_err(|err| format!("cargo build: {err}"))?;
        if !captured.status.success() {
            return Err(format!(
                "cargo build exited with {}: {}",
                captured.status,
                tail(&captured.stderr)
            ));
        }
        let server = executable(&captured.stdout, "keyval-server")
            .ok_or("cargo built no keyval-server executable")?;
        executable(&captured.stdout, "keyval-sim").ok_or("cargo built no keyval-sim executable")?;
        Ok((
            format!("keyval-server and keyval-sim built ({})", server.display()),
            Binaries { server },
        ))
    }

    fn identify(&mut self) -> Result<(String, ()), String> {
        let git = |args: &[&str]| -> Result<String, String> {
            let mut cmd = Command::new("git");
            cmd.current_dir(&self.root).args(args);
            let captured = process::run(&mut cmd, Duration::from_mins(1))
                .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
            if captured.status.success() {
                Ok(captured.stdout.trim().to_owned())
            } else {
                Err(format!(
                    "git {} exited with {}",
                    args.join(" "),
                    captured.status
                ))
            }
        };
        let tree_id = git(&["rev-parse", "HEAD^{tree}"])?;
        let dirty = !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty();
        let inputs = crate::gate::build_inputs(&self.root)?;
        let id = crate::gate::build_id(&tree_id, &inputs);
        let observed = format!(
            "build {id} of tree {tree_id}{}",
            if dirty {
                ", with a dirty working tree"
            } else {
                ""
            }
        );
        self.build = Some(Build {
            id,
            tree: tree_id,
            dirty,
        });
        Ok((observed, ()))
    }

    fn status(&mut self) -> Result<(String, ()), String> {
        let graph = fs::read_to_string(self.root.join("registry/workgraph.toml"))
            .map_err(|err| format!("registry/workgraph.toml: {err}"))?;
        let export = fs::read_to_string(self.root.join(".beads/issues.jsonl")).unwrap_or_default();
        self.items = item_status(&graph, &export)?;
        let m1: Vec<&ItemStatus> = self
            .items
            .iter()
            .filter(|item| item.milestone == "m1")
            .collect();
        let done = m1.iter().filter(|item| item.implemented).count();
        Ok((
            format!(
                "M1: {done} of {} work items implemented; {} items in all",
                m1.len(),
                self.items.len()
            ),
            (),
        ))
    }

    fn init(&mut self, binaries: &Binaries) -> Result<(String, SocketAddr), String> {
        fs::create_dir_all(&self.run_dir)
            .map_err(|err| format!("{}: {err}", self.run_dir.display()))?;
        let client = free_port()?;
        let peer = free_port()?;
        let template = fs::read_to_string(self.root.join(ONE_VOTER))
            .map_err(|err| format!("{ONE_VOTER}: {err}"))?;
        let config = demo_config(&template, &self.cluster_id(), client, peer)?;
        let config_path = self.run_dir.join("one-voter.toml");
        fs::write(&config_path, config)
            .map_err(|err| format!("{}: {err}", config_path.display()))?;
        let instance = instance_id(&self.run_name);
        let mut cmd = Command::new(&binaries.server);
        cmd.args(["init", "--config"]).arg(&config_path).args([
            "--node",
            "1",
            "--instance",
            &instance,
            "--json",
        ]);
        let captured = process::run(&mut cmd, Duration::from_mins(1))
            .map_err(|err| format!("keyval-server init: {err}"))?;
        let report: Value = serde_json::from_str(captured.stdout.trim()).map_err(|err| {
            format!(
                "keyval-server init printed no report ({err}): {}",
                tail(&captured.stderr)
            )
        })?;
        if !captured.status.success() || report["schema"] != "keyval.server.init/v1" {
            return Err(format!(
                "keyval-server init refused ({}): {report}",
                captured.status
            ));
        }
        let address = SocketAddr::from(([127, 0, 0, 1], client));
        let config_shown = config_path.display().to_string();
        self.commands.extend([
            format!("{} init --config {config_shown} --node 1 --instance {instance}", binaries.server.display()),
            format!("{} run --config {config_shown} --node 1", binaries.server.display()),
            format!("curl -s -X PUT 'http://{address}/v1/kv?key=demo/m1' -H 'Idempotency-Key: <32 hex digits>' -H 'Content-Type: application/json' -d '{{\"value\":\"…\"}}'"),
            format!("curl -s 'http://{address}/v1/kv?key=demo/m1'"),
            format!("curl -s 'http://{address}/v1/cluster'"),
            "./dev sim run --seed 42 --profile slice".to_owned(),
        ]);
        Ok((
            format!(
                "node 1 of cluster {} initialised in {} (instance {instance})",
                self.cluster_id(),
                self.run_dir.join("node-1").display()
            ),
            address,
        ))
    }

    /// The run's cluster ID: no other cluster answers to it.
    fn cluster_id(&self) -> String {
        format!("keyval-demo-m1-{}", self.run_name)
    }

    fn start(&mut self, binaries: &Binaries, address: SocketAddr) -> Result<(String, ()), String> {
        let config = self.run_dir.join("one-voter.toml");
        let node = Node::start(&binaries.server, &config, &self.run_dir)?;
        let pid = node.pid();
        self.node = Some(node);
        let deadline = Instant::now() + WAIT;
        loop {
            let ready = get(address, "/v1/health/ready").map(|(status, _)| status == 200);
            let cluster = self.cluster(address);
            if let (Ok(true), Ok(cluster)) = (&ready, &cluster)
                && cluster["role"] == "leader"
            {
                return Ok((
                    format!(
                        "pid {pid} serves http://{address}, ready, leader in term {}",
                        cluster["term"]
                    ),
                    (),
                ));
            }
            if let Some(node) = self.node.as_mut()
                && let Some(status) = node.exited()
            {
                return Err(format!(
                    "keyval-server exited with {status}: {}",
                    node.stderr()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "not ready and leading within {} s: ready {ready:?}, cluster {cluster:?}",
                    WAIT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `/v1/cluster`, refused if another cluster answers on `address`.
    fn cluster(&self, address: SocketAddr) -> Result<Value, String> {
        let (status, body) = get(address, "/v1/cluster")?;
        if status != 200 {
            return Err(format!("/v1/cluster answered {status}: {body}"));
        }
        if body["cluster_id"].as_str() != Some(self.cluster_id().as_str()) {
            return Err(format!("{address} answers as another cluster: {body}"));
        }
        Ok(body)
    }

    fn put(&self, address: SocketAddr) -> Result<(String, u64), String> {
        self.cluster(address)?;
        let request_id = instance_id(&format!("{}-put", self.run_name));
        let body = json!({ "value": self.value() }).to_string();
        let (status, reply) = http(
            address,
            "PUT",
            &format!("/v1/kv?key={KEY}"),
            &[
                ("Idempotency-Key", &request_id),
                ("Content-Type", "application/json"),
            ],
            &body,
        )?;
        let index = reply["index"].as_u64();
        match (status, index, &reply["replayed"]) {
            (200, Some(index), Value::Bool(false)) => Ok((
                format!("200: first executed at index {index} (request {request_id})"),
                index,
            )),
            _ => Err(format!("{status}: {reply}")),
        }
    }

    fn get(&self, address: SocketAddr, value: &str) -> Result<(String, ()), String> {
        self.cluster(address)?;
        let (status, reply) = get(address, &format!("/v1/kv?key={KEY}"))?;
        if status == 200 && reply["value"].as_str() == Some(value) {
            Ok((
                format!("200: {KEY} = {:?}, version {}", value, reply["version"]),
                (),
            ))
        } else {
            Err(format!("{status}: {reply}"))
        }
    }

    fn kill(&mut self) -> Result<(String, ()), String> {
        let node = self.node.as_mut().ok_or("no node is running")?;
        let pid = node.pid();
        let status = node.kill()?;
        self.node = None;
        if status.signal() == Some(9) {
            Ok((
                format!("pid {pid} died of SIGKILL; its receipt is removed"),
                (),
            ))
        } else {
            Err(format!("pid {pid} ended with {status}, not SIGKILL"))
        }
    }

    fn wait_applied(&self, address: SocketAddr, index: u64) -> Result<(String, ()), String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let cluster = self.cluster(address)?;
            if let Some(applied) = cluster["applied_index"].as_u64()
                && applied >= index
            {
                return Ok((format!("applied index {applied} ≥ the PUT's {index}"), ()));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "applied index still below {index} after {} s: {cluster}",
                    WAIT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn sim_run(&self) -> Result<(String, String), String> {
        let request = crate::sim::Request {
            seed: Some(42),
            profile: "slice".to_owned(),
            scenario: None,
            config: crate::sim::config_path(&self.root, None),
        };
        let artifacts = self.root.join("artifacts");
        let report = crate::sim::run(
            &self.root,
            &crate::cargo::Cargo::local(),
            &request,
            &artifacts,
        )
        .map_err(|refusal| refusal.message)?;
        if !report.ok {
            return Err(format!(
                "the simulated slice did not pass: verdict {:?}, failures {:?}",
                report.verdict, report.failures
            ));
        }
        Ok((
            format!(
                "pass: trace {}, {} events, {} lines",
                report.trace.hash, report.trace.events, report.trace.lines
            ),
            report.trace.hash,
        ))
    }

    /// Stops the node if it still runs: a demo that stopped early leaves no process.
    fn stop_node(&mut self) {
        if let Some(mut node) = self.node.take() {
            let _ = node.kill();
        }
    }
}

/// One `keyval-server run` child and its PID receipt.
struct Node {
    child: Option<Child>,
    receipt: PathBuf,
    stderr: PathBuf,
}

impl Node {
    /// Starts `server run` for node 1 of `config`, writing its receipt in `dir`. Refuses if
    /// a receipt is already there: the node it names may still run.
    fn start(server: &Path, config: &Path, dir: &Path) -> Result<Self, String> {
        let receipt = dir.join("node-1.pid.json");
        if receipt.exists() {
            return Err(format!(
                "{} exists: its node may still run",
                receipt.display()
            ));
        }
        let stderr_path = dir.join("node-1.stderr");
        let stderr = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)
            .map_err(|err| format!("{}: {err}", stderr_path.display()))?;
        let child = Command::new(server)
            .args(["run", "--config"])
            .arg(config)
            .args(["--node", "1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|err| format!("keyval-server run: {err}"))?;
        let record =
            json!({ "node": 1, "pid": child.id(), "config": config.display().to_string() });
        let mut node = Self {
            child: Some(child),
            receipt,
            stderr: stderr_path,
        };
        let partial = node.receipt.with_extension("partial");
        let written = fs::write(&partial, record.to_string())
            .and_then(|()| fs::rename(&partial, &node.receipt));
        if let Err(err) = written {
            let _ = node.kill_child();
            return Err(format!("{}: {err}", node.receipt.display()));
        }
        Ok(node)
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    /// The child's exit status, if it has exited on its own.
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
    }

    fn stderr(&self) -> String {
        tail(&fs::read_to_string(&self.stderr).unwrap_or_default())
    }

    /// SIGKILLs the PID the receipt names, only if it is this unreaped child; reaps it and
    /// removes the receipt. Signals nothing otherwise.
    fn kill(&mut self) -> Result<std::process::ExitStatus, String> {
        let child = self.child.as_ref().ok_or("the node is not running")?;
        let named = read_receipt(&self.receipt)?;
        if named != u64::from(child.id()) {
            return Err(format!(
                "{} names pid {named}, not this node's {}: nothing signalled",
                self.receipt.display(),
                child.id()
            ));
        }
        self.kill_child()
    }

    fn kill_child(&mut self) -> Result<std::process::ExitStatus, String> {
        let mut child = self.child.take().ok_or("the node is not running")?;
        child
            .kill()
            .map_err(|err| format!("kill {}: {err}", child.id()))?;
        let status = child
            .wait()
            .map_err(|err| format!("wait {}: {err}", child.id()))?;
        fs::remove_file(&self.receipt)
            .map_err(|err| format!("{}: {err}", self.receipt.display()))?;
        Ok(status)
    }
}

impl Drop for Node {
    /// A node never outlives its demo: the child is still unreaped, so its PID is still ours.
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.kill_child();
        }
    }
}

/// The PID a receipt names.
fn read_receipt(path: &Path) -> Result<u64, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let record: Value =
        serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    record["pid"]
        .as_u64()
        .ok_or_else(|| format!("{}: no pid in {text}", path.display()))
}

/// The work items of `graph` (`registry/workgraph.toml`) with their milestones and titles,
/// each implemented exactly when its bead is closed in `export` (the tracker export).
///
/// # Errors
///
/// The work graph does not parse.
pub fn item_status(graph: &str, export: &str) -> Result<Vec<ItemStatus>, String> {
    #[derive(Deserialize)]
    struct Graph {
        #[serde(default)]
        item: Vec<GraphItem>,
    }
    #[derive(Deserialize)]
    struct GraphItem {
        id: String,
        title: String,
        #[serde(default)]
        milestone: String,
        #[serde(default)]
        bead: String,
    }
    let graph: Graph =
        toml::from_str(graph).map_err(|err| format!("registry/workgraph.toml: {err}"))?;
    let closed: BTreeSet<String> = export
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|bead| bead["status"] == "closed")
        .filter_map(|bead| bead["id"].as_str().map(str::to_owned))
        .collect();
    Ok(graph
        .item
        .into_iter()
        .map(|item| ItemStatus {
            implemented: !item.bead.is_empty() && closed.contains(&item.bead),
            id: item.id,
            title: item.title,
            milestone: item.milestone,
        })
        .collect())
}

/// The one-voter fixture `template` as the demo's config: cluster `cluster`, serving on
/// loopback port `client` (peers on `peer`), its data directory `node-1` beside the file.
///
/// # Errors
///
/// The template no longer has the lines this rewrites.
pub fn demo_config(
    template: &str,
    cluster: &str,
    client: u16,
    peer: u16,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rewritten = 0;
    for line in template.lines() {
        let key = line.split('=').next().unwrap_or_default().trim();
        let replaced = match key {
            "cluster_id" => Some(format!("cluster_id = {cluster:?}")),
            "client_address" => Some(format!("client_address = \"127.0.0.1:{client}\"")),
            "peer_address" => Some(format!("peer_address = \"127.0.0.1:{peer}\"")),
            "data_dir" => Some("data_dir = \"node-1\"".to_owned()),
            _ => None,
        };
        if let Some(line) = replaced {
            rewritten += 1;
            out.push_str(&line);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if rewritten == 4 {
        Ok(out)
    } else {
        Err(format!(
            "{ONE_VOTER} changed: rewrote {rewritten} of its 4 per-run lines"
        ))
    }
}

/// 32 lowercase hex digits derived from `seed`: an instance or request ID unique to a run.
#[must_use]
pub fn instance_id(seed: &str) -> String {
    Sha256::digest(seed.as_bytes())
        .iter()
        .take(16)
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A loopback port nothing listens on now.
fn free_port() -> Result<u16, String> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|err| format!("no free loopback port: {err}"))
}

/// The executable cargo's JSON messages say it built for the binary `name`.
fn executable(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact" && message["target"]["name"] == name
        })
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
}

/// `GET target` from `address`.
fn get(address: SocketAddr, target: &str) -> Result<(u16, Value), String> {
    http(address, "GET", target, &[], "")
}

/// One HTTP/1.1 request to `address`, closing the connection after the response: its status
/// and JSON body.
fn http(
    address: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<(u16, Value), String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|err| format!("connect {address}: {err}"))?;
    stream
        .set_read_timeout(Some(WAIT))
        .map_err(|err| format!("{address}: {err}"))?;
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for (name, value) in headers {
        let _ = write!(raw, "{name}: {value}\r\n");
    }
    if !body.is_empty() {
        let _ = write!(raw, "Content-Length: {}\r\n", body.len());
    }
    raw.push_str("\r\n");
    raw.push_str(body);
    stream
        .write_all(raw.as_bytes())
        .map_err(|err| format!("write {address}: {err}"))?;
    let mut reader = BufReader::new(stream);
    parse_response(&mut reader)
}

/// Reads one HTTP/1.1 response with a `Content-Length` JSON body: its status and body.
fn parse_response(reader: &mut impl BufRead) -> Result<(u16, Value), String> {
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|err| format!("read: {err}"))?;
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| format!("status line {status_line:?}"))?;
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|err| format!("read: {err}"))?
            == 0
        {
            return Err("closed before the headers ended".to_owned());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value
                .trim()
                .parse()
                .map_err(|err| format!("content-length {value:?}: {err}"))?;
        }
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|err| format!("body: {err}"))?;
    let body = serde_json::from_slice(&body).map_err(|err| format!("body JSON: {err}"))?;
    Ok((status, body))
}

/// The last 2 KiB of `text`, at a character boundary.
fn tail(text: &str) -> String {
    let trimmed = text.trim_end();
    let mut start = trimmed.len().saturating_sub(2048);
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed.get(start..).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    // check: DEMO-M1
    use super::*;
    use crate::test_support::scratch_dir;

    const GRAPH: &str = r#"
schema = "keyval.registry.workgraph/v1"

[[item]]
id = "W18"
title = "Production root"
milestone = "m1"
bead = "br-w18"

[[item]]
id = "W19"
title = "M1 owner demo"
milestone = "m1"
bead = "br-w19"

[[item]]
id = "W21"
title = "Peer transport"
milestone = "m2"
bead = ""
"#;

    fn item(id: &str, milestone: &str, implemented: bool) -> ItemStatus {
        ItemStatus {
            id: id.to_owned(),
            title: format!("title of {id}"),
            milestone: milestone.to_owned(),
            implemented,
        }
    }

    fn report(ok: bool) -> DemoReport {
        DemoReport {
            schema: SCHEMA,
            ok,
            label: LABEL,
            milestone: "m1",
            assert: true,
            build: Some(Build {
                id: "sha256:ab".to_owned(),
                tree: "1".repeat(40),
                dirty: true,
            }),
            run_dir: "run".to_owned(),
            steps: Vec::new(),
            sim: Some(SimPair {
                first: "aa".to_owned(),
                second: "bb".to_owned(),
            }),
            items: vec![item("W18", "m1", true), item("W19", "m1", false)],
            commands: vec!["./dev sim run --seed 42 --profile slice".to_owned()],
            limitations: LIMITATIONS.to_vec(),
            step_log: "run/steps.jsonl".to_owned(),
        }
    }

    #[test]
    fn an_item_is_implemented_exactly_when_its_bead_is_closed() {
        let export = concat!(
            "{\"id\":\"br-w18\",\"status\":\"closed\"}\n",
            "{\"id\":\"br-w19\",\"status\":\"in_progress\"}\n",
            "not json\n",
        );
        let items = item_status(GRAPH, export).unwrap();
        let seen: Vec<(&str, bool)> = items
            .iter()
            .map(|i| (i.id.as_str(), i.implemented))
            .collect();
        assert_eq!(
            seen,
            [("W18", true), ("W19", false), ("W21", false)],
            "rule: closed bead = implemented, anything else (open, absent, no bead) = pending (P6); items={items:?}"
        );
    }

    #[test]
    fn a_work_graph_that_does_not_parse_is_an_error_not_an_empty_list() {
        let err = item_status("[[item]]\nid = 7\n", "").unwrap_err();
        assert!(
            err.contains("workgraph.toml"),
            "rule: the error names the file; err={err}"
        );
    }

    #[test]
    fn the_demo_config_rewrites_exactly_the_per_run_lines() {
        let template =
            std::fs::read_to_string(crate::test_support::workspace_root().join(ONE_VOTER)).unwrap();
        let config = demo_config(&template, "keyval-demo-m1-x", 4001, 4002).unwrap();
        for wanted in [
            "cluster_id = \"keyval-demo-m1-x\"",
            "client_address = \"127.0.0.1:4001\"",
            "peer_address = \"127.0.0.1:4002\"",
            "data_dir = \"node-1\"",
            "max_key_bytes = 256",
        ] {
            assert!(
                config.lines().any(|line| line == wanted),
                "rule: {wanted:?} is in the config; config={config}"
            );
        }
        assert!(
            !config.contains("7101"),
            "rule: the fixture's ports are replaced; config={config}"
        );
    }

    #[test]
    fn a_changed_fixture_is_refused_rather_than_half_rewritten() {
        let err = demo_config("cluster_id = \"a\"\n", "c", 1, 2).unwrap_err();
        assert!(
            err.contains("rewrote 1 of its 4"),
            "rule: a fixture missing lines is refused; err={err}"
        );
    }

    #[test]
    fn instance_ids_are_32_hex_digits_and_differ_per_seed() {
        let a = instance_id("run-a");
        assert_eq!(a.len(), 32, "rule: 32 digits; id={a}");
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "rule: lowercase hex; id={a}"
        );
        assert_eq!(
            a,
            instance_id("run-a"),
            "rule: the same seed gives the same ID"
        );
        assert_ne!(
            a,
            instance_id("run-b"),
            "rule: different seeds give different IDs"
        );
    }

    #[test]
    fn responses_parse_with_a_content_length_body() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\ncontent-length: 11\r\n\r\n{\"a\":[1,2]}";
        let (status, body) = parse_response(&mut raw.as_bytes()).unwrap();
        assert_eq!(
            (status, body),
            (200, json!({"a": [1, 2]})),
            "rule: status and JSON body"
        );
        for bad in [
            "",
            "HTTP/1.1 ok\r\n\r\n",
            "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nab",
        ] {
            assert!(
                parse_response(&mut bad.as_bytes()).is_err(),
                "rule: a malformed response is an error; raw={bad:?}"
            );
        }
    }

    #[test]
    fn tail_keeps_the_end_at_a_character_boundary() {
        let text = format!("{}é", "x".repeat(3000));
        let kept = tail(&text);
        assert!(
            kept.ends_with('é') && kept.len() <= 2048,
            "rule: at most 2 KiB of the end; len={}",
            kept.len()
        );
        assert_eq!(
            tail("short\n"),
            "short",
            "rule: short text is kept whole, trimmed"
        );
    }

    #[test]
    fn the_human_report_says_what_is_pending_and_what_is_not_proved() {
        let text = report(true).render_human();
        for wanted in [
            "implemented  W18 title of W18",
            "pending      W19 title of W19",
            "M2: 0 of 0",
            "visible limitations:",
            "one real node only",
            "./dev sim run --seed 42 --profile slice",
            "M1 demo: PASSED",
            "(DIFFERENT)",
            "DIRTY",
        ] {
            if wanted == "M2: 0 of 0" {
                continue;
            }
            assert!(
                text.contains(wanted),
                "rule: the report says {wanted:?} (P6, §11.6); text={text}"
            );
        }
        assert!(
            report(false).render_human().ends_with("M1 demo: FAILED"),
            "rule: a failed demo says so last"
        );
    }

    #[cfg(unix)]
    fn fake_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("fake-server.sh");
        fs::write(&path, "#!/bin/sh\nexec sleep 60\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_node_is_killed_by_its_receipt_and_leaves_none_behind() {
        let dir = scratch_dir("demo-node");
        let server = fake_server(&dir);
        let mut node = Node::start(&server, Path::new("config.toml"), &dir).unwrap();
        let receipt = dir.join("node-1.pid.json");
        assert_eq!(
            read_receipt(&receipt).unwrap(),
            u64::from(node.pid()),
            "rule: the receipt names the child"
        );
        assert!(
            Node::start(&server, Path::new("config.toml"), &dir).is_err(),
            "rule: a receipt that exists refuses a second start"
        );
        let status = node.kill().unwrap();
        assert_eq!(
            status.signal(),
            Some(9),
            "rule: the node dies of SIGKILL; status={status}"
        );
        assert!(
            !receipt.exists(),
            "rule: the receipt is removed with the process"
        );
        assert!(
            node.kill().is_err(),
            "rule: a node already stopped is an error, nothing is signalled"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_receipt_that_names_another_pid_signals_nothing() {
        let dir = scratch_dir("demo-node-foreign");
        let server = fake_server(&dir);
        let mut node = Node::start(&server, Path::new("config.toml"), &dir).unwrap();
        let receipt = dir.join("node-1.pid.json");
        fs::write(&receipt, "{\"node\":1,\"pid\":1}").unwrap();
        let err = node.kill().unwrap_err();
        assert!(
            err.contains("nothing signalled"),
            "rule: a foreign PID is never signalled (rule 18); err={err}"
        );
        assert!(node.exited().is_none(), "rule: the node is untouched");
        fs::write(&receipt, format!("{{\"node\":1,\"pid\":{}}}", node.pid())).unwrap();
        node.kill().unwrap();
    }

    #[test]
    fn the_node_pid_receipt_of_a_dropped_node_is_removed() {
        let dir = scratch_dir("demo-node-drop");
        let server = fake_server(&dir);
        let node = Node::start(&server, Path::new("config.toml"), &dir).unwrap();
        drop(node);
        assert!(
            !dir.join("node-1.pid.json").exists(),
            "rule: a node never outlives its demo"
        );
    }
}
