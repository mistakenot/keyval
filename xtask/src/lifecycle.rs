//! `./dev smoke lifecycle`: the five-node lifecycle smoke test (PLAN.md §10.17, T-14, §12.2
//! W24.3). It runs against **real** `keyval-server` processes started by `./dev cluster`:
//!
//! 1. builds the `keyval` CLI, and writes a per-run config with free loopback ports and a
//!    unique cluster ID under `.local/smoke/lifecycle/<run>/` (never the owner's
//!    `config/local.toml` cluster, rule 18; the run directory is kept afterwards);
//! 2. `./dev cluster init` and `up`, then waits for all five nodes to agree on a leader;
//! 3. writes N acknowledged keys through the leader and remembers the last write's index;
//! 4. `SIGKILL`s the leader and one follower with `./dev cluster kill`, then restarts both
//!    with `./dev cluster restart`;
//! 5. waits until **all five** nodes report an applied index at or beyond the last
//!    acknowledged write's index (`/v1/cluster`): waiting only for the restarted ones
//!    leaves lagging followers to fail the read check spuriously;
//! 6. reads every acknowledged key from every node, through the minimal HTTP client of
//!    [`crate::demo`];
//! 7. `./dev cluster down`, then `keyval doctor` on every **stopped** data directory, bound
//!    to the run's config.
//!
//! Every step is a `keyval.dev.step/v1` record, to stderr and to the run directory's
//! `steps.jsonl`; the first failed step ends the smoke, which exits 1 (W01.6). A smoke that
//! stops early still brings the cluster down; it never deletes a data directory.
//!
//! **What is gated.** Check LIFECYCLE-SMOKE gates the smoke's logic against fake nodes: the
//! config it generates, the leader, catch-up and read judgements and the doctor verdict.
//! The five-process run itself is `./dev smoke lifecycle`, a manual run whose evidence is
//! the run directory (the git-isolated gate candidate cannot build and start five nodes),
//! as for `./dev demo m1` (DEMO-M1).
//!
//! **Claim class.** `SIGKILL` ends a process but not the page cache, so this is evidence
//! of process-crash recovery from real data directories, not of durability across power
//! loss (§10.15 Assumption "the filesystem and disk honour fsync", D-46).

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::clock::Utc;
use crate::cluster::{self, NodeConfig};
use crate::demo::{executable, get, http, instance_id};
use crate::output::Report;
use crate::process;
use crate::steplog::{Outcome, Step, StepLog, StepRecord};

/// Schema identifier of the smoke's report (`schemas/dev/smoke.v1.schema.json`).
pub const SCHEMA: &str = "keyval.dev.smoke/v1";

/// The step log's flow name.
pub const FLOW: &str = "smoke lifecycle";

/// Where the runs go, relative to the workspace root.
pub const RUN_ROOT: &str = ".local/smoke/lifecycle";

/// What the smoke says it is, before anything else (P6).
pub const LABEL: &str = "five-node lifecycle smoke: five real keyval-server processes on this \
                         machine, two killed with SIGKILL and restarted, then every data \
                         directory examined offline";

/// The number of voters the smoke starts (§7.10).
const NODES: usize = 5;

/// How many keys the smoke writes and verifies.
pub const KEYS: usize = 20;

/// The longest any one wait lasts: a leader to be agreed, five nodes to catch up.
const WAIT: Duration = Duration::from_secs(60);

/// The longest one `./dev cluster` command or `keyval doctor` lasts.
const LIMIT: Duration = Duration::from_mins(30);

/// What the smoke admits it cannot show.
pub const LIMITATIONS: &[&str] = &[
    "process-crash evidence only: a SIGKILL ends the process but the page cache survives, so \
     the smoke does not show durability across power loss (D-46, §10.17, §10.15)",
    "one fixed sequence: two nodes killed, the leader and one follower, with no write between \
     the kill and the restart; it is not a random fault campaign",
    "a manual run, not a gated check: the gate judges the smoke's logic against fake nodes \
     (check LIFECYCLE-SMOKE), and the run directory is the evidence of a real run",
];

/// What `./dev smoke lifecycle` was asked to do.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Whether to print each step's outcome to stdout as it ends.
    pub progress: bool,
}

/// The `keyval.dev.smoke/v1` report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SmokeReport {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// Whether every step passed.
    pub ok: bool,
    /// [`LABEL`].
    pub label: &'static str,
    /// The run directory, relative to the workspace root.
    pub run_dir: String,
    /// How many keys were written and verified.
    pub keys: usize,
    /// The index of the last acknowledged write, once there is one.
    pub last_index: Option<u64>,
    /// The nodes killed with SIGKILL, in the order they were killed.
    pub killed: Vec<u64>,
    /// Every step, in order, up to the first failure.
    pub steps: Vec<StepRecord>,
    /// [`LIMITATIONS`].
    pub limitations: Vec<&'static str>,
    /// The step log file.
    pub step_log: String,
}

impl Report for SmokeReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut text = format!("{}\n", self.label);
        for step in &self.steps {
            let mark = match step.outcome {
                Outcome::Passed => "ok  ",
                Outcome::Failed => "FAIL",
            };
            let _ = writeln!(text, "{mark} {}: {}", step.step, step.observed);
        }
        let _ = writeln!(
            text,
            "{}: {} keys, last acknowledged index {}, killed {:?}",
            if self.ok { "passed" } else { "FAILED" },
            self.keys,
            self.last_index
                .map_or_else(|| "none".to_owned(), |index| index.to_string()),
            self.killed
        );
        let _ = write!(
            text,
            "run directory {}; step log {}",
            self.run_dir, self.step_log
        );
        text
    }
}

/// Runs the smoke from the workspace at `root`.
#[must_use]
pub fn run(root: &Path, options: Options) -> SmokeReport {
    let run_name = format!("{}-{}", Utc::now().compact(), std::process::id());
    let run_dir = root.join(RUN_ROOT).join(&run_name);
    let step_log_path = run_dir.join("steps.jsonl");
    let mut smoke = Smoke {
        root: root.to_path_buf(),
        progress: options.progress,
        config: run_dir.join("cluster.toml"),
        state_dir: run_dir.join("state"),
        run_dir: run_dir.clone(),
        run_name,
        log: StepLog::new(FLOW, Some(&step_log_path), true)
            .or_else(|_| StepLog::new(FLOW, None, true))
            .ok(),
        steps: Vec::new(),
        nodes: Vec::new(),
        cluster_id: String::new(),
        up: false,
        last_index: None,
        killed: Vec::new(),
    };
    let ok = smoke.all().is_ok();
    smoke.stop_cluster();
    SmokeReport {
        schema: SCHEMA,
        ok,
        label: LABEL,
        run_dir: relative(root, &run_dir),
        keys: KEYS,
        last_index: smoke.last_index,
        killed: smoke.killed,
        steps: smoke.steps,
        limitations: LIMITATIONS.to_vec(),
        step_log: relative(root, &step_log_path),
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// A step that failed: the smoke stops.
struct Stopped;

/// The smoke's state while it runs.
struct Smoke {
    root: PathBuf,
    progress: bool,
    run_dir: PathBuf,
    run_name: String,
    config: PathBuf,
    state_dir: PathBuf,
    log: Option<StepLog>,
    steps: Vec<StepRecord>,
    nodes: Vec<NodeConfig>,
    cluster_id: String,
    /// Whether `up` started nodes that `down` has not stopped.
    up: bool,
    last_index: Option<u64>,
    killed: Vec<u64>,
}

impl Smoke {
    fn all(&mut self) -> Result<(), Stopped> {
        let rerun = "./dev smoke lifecycle";
        let keyval = self.step(
            "build",
            "cargo build -p cli --bin keyval",
            "the keyval CLI builds",
            rerun,
            |smoke| {
                let path = build_cli(&smoke.root)?;
                Ok((format!("{}", path.display()), path))
            },
        )?;
        self.step(
            "config",
            "write cluster.toml with free ports and a unique cluster ID",
            "five nodes on ten distinct free loopback ports",
            rerun,
            Self::write_config,
        )?;
        self.step(
            "init",
            "./dev cluster init",
            "five data directories stamped with one instance ID",
            rerun,
            |smoke| smoke.cluster("init", None, "stamped"),
        )?;
        self.up = true;
        self.step(
            "up",
            "./dev cluster up",
            "five nodes started",
            rerun,
            |smoke| smoke.cluster("up", None, "started"),
        )?;
        let leader = self.step(
            "leader",
            "poll /v1/cluster on all five nodes",
            "all five agree on one leader that calls itself the leader",
            rerun,
            |smoke| {
                let leader = smoke.wait_leader()?;
                Ok((format!("node {leader} leads"), leader))
            },
        )?;
        let last = self.step(
            "write",
            &format!("PUT {KEYS} keys through node {leader}"),
            "every PUT acknowledged with its log index",
            rerun,
            |smoke| smoke.write_keys(leader),
        )?;
        self.last_index = Some(last);
        self.crash_and_catch_up(leader, last)?;
        self.verify_and_examine(&keyval)
    }

    /// Kills the leader and one follower, restarts them, and waits for all five to catch up.
    fn crash_and_catch_up(&mut self, leader: u64, last: u64) -> Result<(), Stopped> {
        let rerun = "./dev smoke lifecycle";
        let (first, second) = self.step(
            "victims",
            "choose the leader and the lowest-numbered follower",
            "two distinct nodes",
            rerun,
            |smoke| {
                let (first, second) = victims(leader, &smoke.ids())?;
                Ok((
                    format!("the leader {first} and the follower {second}"),
                    (first, second),
                ))
            },
        )?;
        for node in [first, second] {
            self.step(
                "kill",
                &format!("./dev cluster kill --node {node}"),
                "the node's receipted process dies of SIGKILL",
                rerun,
                |smoke| {
                    let observed = smoke.cluster("kill", Some(node), "killed")?;
                    smoke.killed.push(node);
                    Ok(observed)
                },
            )?;
        }
        for node in [first, second] {
            self.step(
                "restart",
                &format!("./dev cluster restart --node {node}"),
                "the node starts again from its data directory",
                rerun,
                |smoke| smoke.cluster("restart", Some(node), "started"),
            )?;
        }
        self.step(
            "catch-up",
            "poll /v1/cluster on all five nodes",
            &format!("every node's applied index ≥ the last acknowledged write's index {last}"),
            rerun,
            |smoke| smoke.wait_applied(last),
        )?;
        Ok(())
    }

    /// Reads every key from every node, stops the cluster, and examines every data directory.
    fn verify_and_examine(&mut self, keyval: &Path) -> Result<(), Stopped> {
        let rerun = "./dev smoke lifecycle";
        self.step(
            "read",
            &format!("GET {KEYS} keys from each of the five nodes"),
            "every acknowledged key reads back its value from every node",
            rerun,
            |smoke| smoke.verify_reads(),
        )?;
        self.step(
            "down",
            "./dev cluster down",
            "all five nodes stopped, no data deleted",
            rerun,
            |smoke| {
                let observed = smoke.cluster("down", None, "stopped")?;
                smoke.up = false;
                Ok(observed)
            },
        )?;
        self.step(
            "doctor",
            "keyval doctor <data dir> --config cluster.toml --node N --json",
            "every stopped data directory is clean or has only a torn tail, bound to the config",
            rerun,
            |smoke| smoke.doctor_all(keyval),
        )?;
        Ok(())
    }

    /// Runs one step: times it, logs it, keeps its record.
    fn step<T>(
        &mut self,
        name: &str,
        action: &str,
        expected: &str,
        rerun: &str,
        body: impl FnOnce(&mut Self) -> Result<(String, T), String>,
    ) -> Result<T, Stopped> {
        let started = Instant::now();
        let (observed, value) = match body(self) {
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
        if self.progress {
            let mark = match record.outcome {
                Outcome::Passed => "ok  ",
                Outcome::Failed => "FAIL",
            };
            let _ = writeln!(io::stdout().lock(), "{mark} {name}: {}", record.observed);
        }
        self.steps.push(record);
        value.ok_or(Stopped)
    }

    fn ids(&self) -> Vec<u64> {
        self.nodes.iter().map(|node| node.id).collect()
    }

    fn write_config(&mut self) -> Result<(String, ()), String> {
        let template = fs::read_to_string(self.root.join("config/local.toml"))
            .map_err(|err| format!("config/local.toml: {err}"))?;
        let ports = free_ports(2 * NODES)?;
        self.cluster_id = format!("smoke-{}", self.run_name);
        let text = generate_config(&template, &self.cluster_id, &ports)?;
        fs::create_dir_all(&self.run_dir)
            .map_err(|err| format!("{}: {err}", self.run_dir.display()))?;
        fs::write(&self.config, text).map_err(|err| format!("{}: {err}", self.config.display()))?;
        self.nodes = cluster::load_config(&self.config)?;
        Ok((
            format!(
                "{} (cluster {})",
                relative(&self.root, &self.config),
                self.cluster_id
            ),
            (),
        ))
    }

    /// `./dev cluster <action>` as the subprocess a person would run. `state` is the state
    /// every node the command reports on must be in, and the observed text names each.
    fn cluster(
        &self,
        action: &str,
        node: Option<u64>,
        state: &str,
    ) -> Result<(String, ()), String> {
        let myself = std::env::current_exe().map_err(|err| format!("the ./dev binary: {err}"))?;
        let mut cmd = Command::new(myself);
        cmd.current_dir(&self.root)
            .args(["--json", "cluster", action])
            .arg("--config")
            .arg(&self.config)
            .arg("--state-dir")
            .arg(&self.state_dir);
        if let Some(node) = node {
            cmd.arg("--node").arg(node.to_string());
        }
        let captured =
            process::run(&mut cmd, LIMIT).map_err(|err| format!("cluster {action}: {err}"))?;
        let report: Value = serde_json::from_str(captured.stdout.trim()).map_err(|err| {
            format!(
                "cluster {action}: no JSON report ({err}): {}",
                captured.stderr
            )
        })?;
        judge_cluster(&report, action, node, state)
    }

    fn views(&self) -> Vec<(u64, Result<Value, String>)> {
        self.nodes
            .iter()
            .map(|node| (node.id, view(node.client, &self.cluster_id)))
            .collect()
    }

    fn wait_leader(&self) -> Result<u64, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let views = self.views();
            match agreed_leader(&views) {
                Ok(leader) => return Ok(leader),
                Err(why) if Instant::now() >= deadline => {
                    return Err(format!("no agreed leader after {}s: {why}", WAIT.as_secs()));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    fn address(&self, node: u64) -> Result<SocketAddr, String> {
        self.nodes
            .iter()
            .find(|candidate| candidate.id == node)
            .map(|candidate| candidate.client)
            .ok_or_else(|| format!("node {node} is not in the config"))
    }

    fn write_keys(&self, leader: u64) -> Result<(String, u64), String> {
        let address = self.address(leader)?;
        let mut last = 0;
        for i in 0..KEYS {
            let request = instance_id(&format!("{}-put-{i}", self.run_name));
            let body = serde_json::json!({ "value": value(&self.run_name, i) }).to_string();
            let reply = http(
                address,
                "PUT",
                &format!("/v1/kv?key={}", key(i)),
                &[
                    ("Idempotency-Key", &request),
                    ("Content-Type", "application/json"),
                ],
                &body,
            );
            last = acknowledged(i, reply)?;
        }
        Ok((
            format!("{KEYS} keys acknowledged; the last at index {last}"),
            last,
        ))
    }

    fn wait_applied(&self, index: u64) -> Result<(String, ()), String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let views = self.views();
            let short = shortfall(&views, index);
            if short.is_empty() {
                let applied: Vec<String> = views
                    .iter()
                    .filter_map(|(id, view)| {
                        view.as_ref()
                            .ok()
                            .map(|view| format!("{id}:{}", view["applied_index"]))
                    })
                    .collect();
                return Ok((
                    format!("applied indexes {} ≥ {index}", applied.join(" ")),
                    (),
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "after {}s, not at index {index}: {}",
                    WAIT.as_secs(),
                    short.join("; ")
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn verify_reads(&self) -> Result<(String, ()), String> {
        let problems = read_problems(&self.nodes, &self.run_name);
        if problems.is_empty() {
            Ok((format!("{} reads matched", KEYS * self.nodes.len()), ()))
        } else {
            Err(problems.join("; "))
        }
    }

    fn doctor_all(&self, keyval: &Path) -> Result<(String, ()), String> {
        let mut seen = Vec::new();
        let mut problems = Vec::new();
        for node in &self.nodes {
            let mut cmd = Command::new(keyval);
            cmd.current_dir(&self.root)
                .arg("doctor")
                .arg(&node.data_dir)
                .arg("--config")
                .arg(&self.config)
                .args(["--node", &node.id.to_string(), "--json"]);
            let judged = process::run(&mut cmd, LIMIT)
                .map_err(|err| format!("node {}: keyval doctor: {err}", node.id))
                .and_then(|captured| {
                    let report: Value =
                        serde_json::from_str(captured.stdout.trim()).map_err(|err| {
                            format!(
                                "node {}: no JSON report ({err}): {}",
                                node.id, captured.stderr
                            )
                        })?;
                    doctor_verdict(node.id, captured.status.success(), &report)
                });
            match judged {
                Ok(verdict) => seen.push(format!("{}:{verdict}", node.id)),
                Err(why) => problems.push(why),
            }
        }
        if problems.is_empty() {
            Ok((format!("doctor verdicts {}", seen.join(" ")), ()))
        } else {
            Err(problems.join("; "))
        }
    }

    /// Brings the cluster down if a failed step left it up. Best effort: the failure that
    /// stopped the smoke is already in the log.
    fn stop_cluster(&mut self) {
        if self.up {
            drop(self.cluster("down", None, "stopped"));
            self.up = false;
        }
    }
}

// --- Judgements ------------------------------------------------------------------------

/// The key the smoke writes as number `i`.
#[must_use]
pub fn key(i: usize) -> String {
    format!("smoke/lifecycle/{i:03}")
}

/// The value the smoke writes as number `i`: unique to the run and to the key.
#[must_use]
pub fn value(run: &str, i: usize) -> String {
    format!("lifecycle smoke {run} value {i}")
}

/// The config for one run: the template's top-level settings, with `cluster_id` replaced,
/// and five nodes on `ports` (the first five are client ports, the last five peer ports),
/// each with its data directory under `nodes/` next to the config.
///
/// # Errors
///
/// The template has no `cluster_id` line or no `[[nodes]]`, or `ports` is not ten ports.
pub fn generate_config(template: &str, cluster_id: &str, ports: &[u16]) -> Result<String, String> {
    if ports.len() != 2 * NODES {
        return Err(format!(
            "{} ports; the smoke needs {}",
            ports.len(),
            2 * NODES
        ));
    }
    let head = template
        .split_once("[[nodes]]")
        .map(|(head, _)| head)
        .ok_or("the template has no [[nodes]]")?;
    let mut replaced = false;
    let mut text = String::new();
    for line in head.lines() {
        if line.trim_start().starts_with("cluster_id") && line.contains('=') {
            let _ = writeln!(text, "cluster_id = \"{cluster_id}\"");
            replaced = true;
        } else {
            let _ = writeln!(text, "{line}");
        }
    }
    if !replaced {
        return Err("the template has no cluster_id line".to_owned());
    }
    for id in 1..=NODES {
        let _ = write!(
            text,
            "[[nodes]]\nid = {id}\nclient_address = \"127.0.0.1:{}\"\n\
             peer_address = \"127.0.0.1:{}\"\ndata_dir = \"nodes/{id}\"\n\n",
            ports[id - 1],
            ports[id - 1 + NODES]
        );
    }
    Ok(text)
}

/// The leader all five views agree on, and which calls itself the leader.
///
/// # Errors
///
/// A node is unreachable, has no leader, names another one, or the named leader does not
/// call itself the leader.
pub fn agreed_leader(views: &[(u64, Result<Value, String>)]) -> Result<u64, String> {
    let mut leader = None;
    let mut roles = Vec::new();
    for (id, view) in views {
        let view = view.as_ref().map_err(|why| format!("node {id}: {why}"))?;
        let named = view["leader_id"]
            .as_u64()
            .ok_or_else(|| format!("node {id} knows no leader"))?;
        match leader {
            None => leader = Some(named),
            Some(first) if first != named => {
                return Err(format!("node {id} names leader {named}, others {first}"));
            }
            Some(_) => {}
        }
        roles.push((*id, view["role"].as_str().unwrap_or("?").to_owned()));
    }
    let leader = leader.ok_or("no nodes")?;
    match roles.iter().find(|(id, _)| *id == leader) {
        Some((_, role)) if role == "leader" => Ok(leader),
        Some((_, role)) => Err(format!(
            "node {leader} is named leader but its role is {role}"
        )),
        None => Err(format!("leader {leader} is not among the nodes")),
    }
}

/// The two nodes to kill: `leader` and the lowest-numbered other node.
///
/// # Errors
///
/// `leader` is not among `ids`, or there is no other node.
pub fn victims(leader: u64, ids: &[u64]) -> Result<(u64, u64), String> {
    if !ids.contains(&leader) {
        return Err(format!("leader {leader} is not among the nodes {ids:?}"));
    }
    ids.iter()
        .copied()
        .filter(|id| *id != leader)
        .min()
        .map(|follower| (leader, follower))
        .ok_or_else(|| format!("no follower among the nodes {ids:?}"))
}

/// `GET /v1/cluster` from `address`, refused if another cluster answers there.
fn view(address: SocketAddr, cluster_id: &str) -> Result<Value, String> {
    let (status, body) = get(address, "/v1/cluster")?;
    if status != 200 {
        return Err(format!("/v1/cluster answered {status}: {body}"));
    }
    if body["cluster_id"].as_str() != Some(cluster_id) {
        return Err(format!("{address} answers as another cluster: {body}"));
    }
    Ok(body)
}

/// The log index a PUT was acknowledged at.
///
/// # Errors
///
/// The request failed, the answer was not `200` with a first-executed index, or it was a
/// replay.
pub fn acknowledged(i: usize, reply: Result<(u16, Value), String>) -> Result<u64, String> {
    let (status, body) = reply.map_err(|why| format!("PUT of key {i}: {why}"))?;
    match (status, body["index"].as_u64(), &body["replayed"]) {
        (200, Some(index), Value::Bool(false)) => Ok(index),
        _ => Err(format!("PUT of key {i} answered {status}: {body}")),
    }
}

/// What keeps the nodes from all being at `index`: one entry per node that is unreachable
/// or whose applied index is lower. Empty when all of them have reached it.
#[must_use]
pub fn shortfall(views: &[(u64, Result<Value, String>)], index: u64) -> Vec<String> {
    views
        .iter()
        .filter_map(|(id, view)| match view {
            Err(why) => Some(format!("node {id}: {why}")),
            Ok(view) => match view["applied_index"].as_u64() {
                Some(applied) if applied >= index => None,
                Some(applied) => Some(format!("node {id} applied {applied}")),
                None => Some(format!("node {id} reports no applied index: {view}")),
            },
        })
        .collect()
}

/// Why a read of `key` from `node` is not the acknowledged value, if it is not.
#[must_use]
pub fn read_problem(
    node: u64,
    key: &str,
    expected: &str,
    reply: &Result<(u16, Value), String>,
) -> Option<String> {
    match reply {
        Err(why) => Some(format!("node {node} key {key}: {why}")),
        Ok((200, body)) if body["value"].as_str() == Some(expected) => None,
        Ok((status, body)) => Some(format!(
            "node {node} key {key}: answered {status} {body}, expected value {expected:?}"
        )),
    }
}

/// Every read problem across all keys and nodes.
fn read_problems(nodes: &[NodeConfig], run: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for node in nodes {
        for i in 0..KEYS {
            let reply = get(node.client, &format!("/v1/kv?key={}", key(i)));
            problems.extend(read_problem(node.id, &key(i), &value(run, i), &reply));
        }
    }
    problems
}

/// The verdict of one `keyval doctor --json` report, if the directory may start a node.
///
/// # Errors
///
/// The doctor exited non-zero, its verdict is neither `clean` nor `discard_tail`, or the
/// genesis binding was not verified against the run's config.
pub fn doctor_verdict(node: u64, exited_ok: bool, report: &Value) -> Result<String, String> {
    let verdict = report["verdict"].as_str().unwrap_or("?");
    let binding = report["binding"].as_str().unwrap_or("?");
    if !exited_ok || !["clean", "discard_tail"].contains(&verdict) || binding != "verified" {
        return Err(format!(
            "node {node}: doctor exited ok={exited_ok}, verdict {verdict}, binding {binding}: {}",
            report["would_do"]
        ));
    }
    Ok(verdict.to_owned())
}

/// Judges a `keyval.dev.cluster/v1` report: `ok`, and the nodes the command is about in
/// `state` (`node` alone, or all of them).
fn judge_cluster(
    report: &Value,
    action: &str,
    node: Option<u64>,
    state: &str,
) -> Result<(String, ()), String> {
    if report["ok"] != Value::Bool(true) {
        return Err(format!("cluster {action} failed: {}", report["error"]));
    }
    let nodes = report["nodes"].as_array().map_or(&[][..], Vec::as_slice);
    let wanted: Vec<&Value> = nodes
        .iter()
        .filter(|entry| node.is_none_or(|node| entry["node"].as_u64() == Some(node)))
        .collect();
    if wanted.is_empty() {
        return Err(format!("cluster {action} reported no node: {report}"));
    }
    let wrong: Vec<String> = wanted
        .iter()
        .filter(|entry| entry["state"].as_str() != Some(state))
        .map(|entry| format!("node {} is {}", entry["node"], entry["state"]))
        .collect();
    if !wrong.is_empty() {
        return Err(format!(
            "cluster {action}: expected {state}; {}",
            wrong.join(", ")
        ));
    }
    let detail: Vec<String> = wanted
        .iter()
        .map(|entry| format!("{}:{}", entry["node"], entry["pid"]))
        .collect();
    Ok((format!("{state} (node:pid) {}", detail.join(" ")), ()))
}

/// `count` distinct loopback ports nothing listens on now: every probe listener stays open
/// until all are chosen, so the kernel cannot hand one port out twice.
fn free_ports(count: usize) -> Result<Vec<u16>, String> {
    let mut held = Vec::with_capacity(count);
    for _ in 0..count {
        held.push(
            TcpListener::bind(("127.0.0.1", 0))
                .map_err(|err| format!("no free loopback port: {err}"))?,
        );
    }
    held.iter()
        .map(|listener| {
            listener
                .local_addr()
                .map(|address| address.port())
                .map_err(|err| format!("no free loopback port: {err}"))
        })
        .collect()
}

/// Builds the `keyval` CLI and returns its path.
fn build_cli(root: &Path) -> Result<PathBuf, String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args([
        "build",
        "--locked",
        "--quiet",
        "--message-format=json",
        "-p",
        "cli",
        "--bin",
        "keyval",
    ]);
    let captured = process::run(&mut cmd, LIMIT).map_err(|err| format!("cargo build: {err}"))?;
    if !captured.status.success() {
        return Err(format!(
            "cargo build exited with {}: {}",
            captured.status, captured.stderr
        ));
    }
    executable(&captured.stdout, "keyval")
        .ok_or_else(|| "cargo built no keyval executable".to_owned())
}

// check: LIFECYCLE-SMOKE
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_dir;
    use std::io::{BufRead as _, BufReader, Read as _};
    use std::net::TcpStream;
    use std::thread;

    const TEMPLATE: &str = include_str!("../../config/local.toml");

    fn ports() -> Vec<u16> {
        free_ports(10).unwrap()
    }

    fn cluster_view(
        id: u64,
        role: &str,
        leader: Option<u64>,
        applied: u64,
    ) -> (u64, Result<Value, String>) {
        (
            id,
            Ok(serde_json::json!({
                "node_id": id, "role": role, "leader_id": leader, "applied_index": applied,
            })),
        )
    }

    /// A fake node: answers `GET /v1/kv?key=…` from `kv` and `/v1/cluster` with a fixed
    /// body, one connection at a time, until the test process ends.
    fn fake_node(kv: Vec<(String, String)>, applied: u64) -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim_end().is_empty() {
                        break;
                    }
                }
                let target = request.split(' ').nth(1).unwrap_or("");
                let (status, body) = if target == "/v1/cluster" {
                    (
                        200,
                        serde_json::json!({ "cluster_id": "t", "applied_index": applied }),
                    )
                } else {
                    let found = kv
                        .iter()
                        .find(|(key, _)| target.ends_with(&format!("key={key}")))
                        .map(|(_, value)| value.clone());
                    found.map_or_else(
                        || (404, serde_json::json!({ "error": { "code": "not_found" } })),
                        |value| (200, serde_json::json!({ "value": value, "version": 1 })),
                    )
                };
                let body = body.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let mut stream = reader.into_inner();
                let _ = stream.write_all(reply.as_bytes());
                let mut sink = Vec::new();
                let _ = stream.read_to_end(&mut sink);
                drop(TcpStream::shutdown(&stream, std::net::Shutdown::Both));
            }
        });
        address
    }

    fn node(id: u64, client: SocketAddr) -> NodeConfig {
        NodeConfig {
            id,
            data_dir: PathBuf::from(format!("nodes/{id}")),
            client,
            peer: client,
        }
    }

    #[test]
    fn the_generated_config_loads_with_five_nodes_on_distinct_ports_and_a_unique_cluster() {
        let ports = ports();
        let text = generate_config(TEMPLATE, "smoke-run-1", &ports).unwrap();
        assert!(
            text.contains("cluster_id = \"smoke-run-1\"") && !text.contains("keyval-local-v1"),
            "rule: the cluster ID is the run's; text={text}"
        );
        assert!(
            text.contains("max_key_bytes = 256") && text.contains("tick_ms = 50"),
            "rule: the template's settings are kept; text={text}"
        );
        let dir = scratch_dir("lifecycle-config");
        fs::write(dir.join("cluster.toml"), &text).unwrap();
        let nodes = cluster::load_config(&dir.join("cluster.toml")).unwrap();
        assert_eq!(nodes.len(), 5, "rule: five nodes; nodes={nodes:?}");
        let mut seen: Vec<u16> = nodes
            .iter()
            .flat_map(|n| [n.client.port(), n.peer.port()])
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 10, "rule: ten distinct ports; nodes={nodes:?}");
        assert!(
            nodes.iter().all(|n| n.data_dir.starts_with(&dir)),
            "rule: data directories live next to the config; nodes={nodes:?}"
        );
    }

    #[test]
    fn a_template_without_nodes_or_a_cluster_id_or_with_few_ports_is_refused() {
        let ports = ports();
        for (template, ports, what) in [
            ("cluster_id = \"x\"\n", ports.as_slice(), "no [[nodes]]"),
            (
                "tick_ms = 50\n[[nodes]]\nid = 1\n",
                ports.as_slice(),
                "no cluster_id",
            ),
            (TEMPLATE, &ports[..9], "nine ports"),
        ] {
            let result = generate_config(template, "smoke-x", ports);
            assert!(result.is_err(), "rule: refused ({what}); got {result:?}");
        }
    }

    #[test]
    fn keys_and_values_are_distinct_and_values_are_unique_to_the_run() {
        let keys: Vec<String> = (0..KEYS).map(key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), KEYS, "rule: distinct keys; keys={keys:?}");
        assert_ne!(value("a", 1), value("b", 1), "rule: a value names its run");
        assert_ne!(value("a", 1), value("a", 2), "rule: a value names its key");
    }

    #[test]
    fn a_leader_is_agreed_only_when_every_node_names_it_and_it_says_so() {
        let agreed = |leader| {
            (1..=5)
                .map(|id| {
                    cluster_view(
                        id,
                        if id == 3 { "leader" } else { "follower" },
                        Some(leader),
                        9,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            agreed_leader(&agreed(3)),
            Ok(3),
            "rule: all name node 3, which leads"
        );

        let mut split = agreed(3);
        split[4] = cluster_view(5, "follower", Some(2), 9);
        let why = agreed_leader(&split).unwrap_err();
        assert!(
            why.contains("node 5 names leader 2"),
            "rule: a split view is named; why={why}"
        );

        let mut none = agreed(3);
        none[0] = cluster_view(1, "candidate", None, 9);
        let why = agreed_leader(&none).unwrap_err();
        assert!(
            why.contains("node 1 knows no leader"),
            "rule: no leader is named; why={why}"
        );

        let why = agreed_leader(&agreed(4)).unwrap_err();
        assert!(
            why.contains("is not among") || why.contains("role"),
            "rule: a leader that is not one is refused; why={why}"
        );

        let mut down = agreed(3);
        down[1] = (2, Err("connect refused".to_owned()));
        let why = agreed_leader(&down).unwrap_err();
        assert!(
            why.contains("node 2: connect refused"),
            "rule: an unreachable node blocks agreement; why={why}"
        );
    }

    #[test]
    fn the_victims_are_the_leader_and_the_lowest_numbered_follower() {
        let ids = [1, 2, 3, 4, 5];
        assert_eq!(
            victims(1, &ids),
            Ok((1, 2)),
            "rule: leader 1 pairs with follower 2"
        );
        assert_eq!(
            victims(3, &ids),
            Ok((3, 1)),
            "rule: leader 3 pairs with follower 1"
        );
        assert!(
            victims(9, &ids).is_err(),
            "rule: a leader outside the config is refused"
        );
        assert!(
            victims(1, &[1]).is_err(),
            "rule: a lone node has no follower"
        );
    }

    #[test]
    fn catch_up_names_every_node_that_is_behind_or_unreachable() {
        let views = vec![
            cluster_view(1, "leader", Some(1), 12),
            cluster_view(2, "follower", Some(1), 11),
            cluster_view(3, "follower", Some(1), 12),
            (4, Err("connect refused".to_owned())),
            cluster_view(5, "follower", Some(1), 40),
        ];
        let short = shortfall(&views, 12);
        assert_eq!(
            short.len(),
            2,
            "rule: nodes 2 and 4 are short; short={short:?}"
        );
        assert!(short[0].contains("node 2 applied 11"), "short={short:?}");
        assert!(
            short[1].contains("node 4: connect refused"),
            "short={short:?}"
        );
        assert!(
            shortfall(&views[..1], 12).is_empty(),
            "rule: at the index is enough"
        );
        assert_eq!(
            shortfall(&views[2..3], 13).len(),
            1,
            "rule: one short of the index is short"
        );
    }

    #[test]
    fn a_put_is_acknowledged_only_as_a_first_execution_with_an_index() {
        let ok = serde_json::json!({ "index": 17, "replayed": false });
        assert_eq!(acknowledged(0, Ok((200, ok))), Ok(17));
        let replay = serde_json::json!({ "index": 17, "replayed": true });
        assert!(
            acknowledged(0, Ok((200, replay))).is_err(),
            "rule: a replay is not a first write"
        );
        let refused = serde_json::json!({ "error": { "code": "not_leader" } });
        assert!(
            acknowledged(0, Ok((503, refused))).is_err(),
            "rule: a refusal is not an acknowledgement"
        );
        assert!(
            acknowledged(0, Err("reset".to_owned())).is_err(),
            "rule: a failed request is not one"
        );
    }

    #[test]
    fn reads_across_real_sockets_name_the_node_and_key_that_miss_or_differ() {
        let run = "r";
        let all: Vec<(String, String)> = (0..KEYS).map(|i| (key(i), value(run, i))).collect();
        let mut missing = all.clone();
        missing.remove(7);
        let mut wrong = all.clone();
        wrong[3].1 = "stale".to_owned();
        let nodes = vec![
            node(1, fake_node(all, 5)),
            node(2, fake_node(missing, 5)),
            node(3, fake_node(wrong, 5)),
        ];
        let problems = read_problems(&nodes, run);
        assert_eq!(
            problems.len(),
            2,
            "rule: exactly the two bad reads are named; problems={problems:?}"
        );
        assert!(
            problems[0].contains("node 2") && problems[0].contains(&key(7)),
            "problems={problems:?}"
        );
        assert!(
            problems[1].contains("node 3") && problems[1].contains(&key(3)),
            "problems={problems:?}"
        );
        assert!(
            read_problems(&nodes[..1], run).is_empty(),
            "rule: a node holding every key passes"
        );
    }

    #[test]
    fn a_cluster_view_over_a_real_socket_is_refused_when_another_cluster_answers() {
        let address = fake_node(Vec::new(), 4);
        let seen = view(address, "t").unwrap();
        assert_eq!(
            seen["applied_index"], 4,
            "rule: the view is the node's body; seen={seen}"
        );
        let why = view(address, "other").unwrap_err();
        assert!(
            why.contains("another cluster"),
            "rule: a stranger on the port is refused; why={why}"
        );
    }

    #[test]
    fn a_doctor_verdict_passes_only_for_a_startable_verified_directory() {
        let report = |verdict: &str, binding: &str| serde_json::json!({ "verdict": verdict, "binding": binding, "would_do": "w" });
        assert_eq!(
            doctor_verdict(1, true, &report("clean", "verified")),
            Ok("clean".to_owned())
        );
        assert_eq!(
            doctor_verdict(1, true, &report("discard_tail", "verified")),
            Ok("discard_tail".to_owned()),
            "rule: a torn tail is startable"
        );
        for (exited, verdict, binding) in [
            (true, "refuse", "verified"),
            (true, "live", "verified"),
            (true, "no_journal", "verified"),
            (true, "clean", "unchecked"),
            (true, "clean", "mismatch"),
            (false, "clean", "verified"),
        ] {
            let result = doctor_verdict(2, exited, &report(verdict, binding));
            assert!(
                result.is_err(),
                "rule: refused ({exited}, {verdict}, {binding}); got {result:?}"
            );
        }
    }

    #[test]
    fn a_cluster_report_is_judged_on_ok_and_the_state_of_the_nodes_asked_about() {
        let report = serde_json::json!({
            "ok": true,
            "nodes": [
                { "node": 1, "state": "killed", "pid": 11 },
                { "node": 2, "state": "running", "pid": 12 },
            ],
        });
        assert!(
            judge_cluster(&report, "kill", Some(1), "killed").is_ok(),
            "rule: node 1 is killed"
        );
        let why = judge_cluster(&report, "kill", Some(2), "killed").unwrap_err();
        assert!(
            why.contains("node 2 is"),
            "rule: the wrong state is named; why={why}"
        );
        assert!(
            judge_cluster(&report, "down", None, "stopped").is_err(),
            "rule: every node must match"
        );
        assert!(
            judge_cluster(&report, "kill", Some(9), "killed").is_err(),
            "rule: an unlisted node is not killed"
        );
        let failed = serde_json::json!({ "ok": false, "error": "boom", "nodes": [] });
        assert!(
            judge_cluster(&failed, "up", None, "started")
                .unwrap_err()
                .contains("boom")
        );
    }

    #[test]
    fn a_report_renders_every_step_and_says_how_it_ended() {
        let step = |outcome| StepRecord {
            schema: crate::steplog::SCHEMA,
            flow: FLOW.to_owned(),
            step: "leader".to_owned(),
            action: "a".to_owned(),
            expected: "e".to_owned(),
            observed: "node 2 leads".to_owned(),
            outcome,
            duration_ms: 1,
            rerun: None,
        };
        let report = |ok, outcome| SmokeReport {
            schema: SCHEMA,
            ok,
            label: LABEL,
            run_dir: ".local/smoke/lifecycle/r".to_owned(),
            keys: KEYS,
            last_index: Some(30),
            killed: vec![2, 1],
            steps: vec![step(outcome)],
            limitations: LIMITATIONS.to_vec(),
            step_log: ".local/smoke/lifecycle/r/steps.jsonl".to_owned(),
        };
        let passed = report(true, Outcome::Passed).render_human();
        assert!(
            passed.contains("ok   leader: node 2 leads")
                && passed.contains("passed: 20 keys, last acknowledged index 30"),
            "text={passed}"
        );
        let failed = report(false, Outcome::Failed).render_human();
        assert!(
            failed.contains("FAIL leader") && failed.contains("FAILED"),
            "text={failed}"
        );
    }
}
