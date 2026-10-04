//! PID-receipt helpers for one `keyval-server run` process (W18.4): start it and record a
//! receipt, SIGKILL exactly the PID that receipt names, and start it again on the same
//! config. Deliberately minimal: the launcher with foreign-process safety across
//! invocations is `./dev cluster` (W22, O-2).
//!
//! The receipt exists exactly while its PID is a child this helper has not reaped, so the
//! kernel cannot have recycled it: `kill` signals only that PID, and reaps the child before
//! removing the receipt (rule 18).
#![cfg(test)]

use std::fs::{self, OpenOptions};
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::support::{Reply, request, send, try_read_reply};

/// SIGKILL's number: `Child::kill` sends it on Unix.
pub const SIGKILL: i32 = 9;

/// What to start: node `node` of the config at `config`, whose `cluster_id` is `cluster`,
/// serving on `address`, logging to `log`, with its receipt and stderr in `dir`.
#[derive(Clone, Debug)]
pub struct Spec {
    pub config: PathBuf,
    pub node: u64,
    pub cluster: String,
    pub address: SocketAddr,
    pub log: PathBuf,
    pub dir: PathBuf,
}

/// Why `kill` signalled nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No child is running: it was already killed.
    NotRunning,
    /// The receipt is missing or unreadable.
    NoReceipt(String),
    /// The receipt names a PID that is not the child this helper started.
    OtherPid { receipt: u64, child: u32 },
}

/// A started node: its child process while it runs, and its receipt.
pub struct Node {
    pub spec: Spec,
    child: Option<Child>,
    starts: u32,
}

impl Node {
    /// Starts the node and writes its receipt. A fixture failure if a receipt already
    /// exists: the node it names may still be running.
    pub fn start(spec: Spec) -> Self {
        let mut node = Self {
            spec,
            child: None,
            starts: 0,
        };
        node.spawn();
        node
    }

    /// The receipt's path.
    pub fn receipt(&self) -> PathBuf {
        self.spec
            .dir
            .join(format!("node-{}.pid.json", self.spec.node))
    }

    /// The running child's PID.
    pub fn pid(&self) -> u32 {
        self.child
            .as_ref()
            .expect("fixture: the node is running")
            .id()
    }

    /// Everything the node's processes wrote to stderr so far, for failure messages.
    pub fn stderr(&self) -> String {
        fs::read_to_string(self.stderr_path()).unwrap_or_default()
    }

    /// SIGKILLs the PID the receipt names, only if it is the running child, then reaps it and
    /// removes the receipt. Signals nothing otherwise.
    pub fn kill(&mut self) -> Result<ExitStatus, Refusal> {
        let path = self.receipt();
        let child = self.child.as_mut().ok_or(Refusal::NotRunning)?;
        let receipt = read_receipt(&path)?;
        if receipt != u64::from(child.id()) {
            return Err(Refusal::OtherPid {
                receipt,
                child: child.id(),
            });
        }
        child.kill().unwrap();
        let status = child.wait().unwrap();
        self.child = None;
        fs::remove_file(path).unwrap();
        Ok(status)
    }

    /// Starts the killed node again, with a fresh receipt.
    pub fn restart(&mut self) {
        assert!(self.child.is_none(), "fixture: restart follows a kill");
        self.spawn();
    }

    /// Waits until the node is ready and, for a one-voter config, leads; returns `/v1/cluster`.
    pub fn wait_leading(&self) -> Reply {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if TcpStream::connect(self.spec.address).is_ok() {
                let cluster = self.cluster();
                let ready = send(
                    self.spec.address,
                    &request("GET", "/v1/health/ready", &[], ""),
                );
                if ready.status == 200 && cluster.body["role"] == "leader" {
                    return cluster;
                }
            }
            assert!(
                Instant::now() < deadline,
                "fixture: node {} leads within 20 s of start {}; stderr: {}",
                self.spec.node,
                self.starts,
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until `/v1/cluster` reports an applied index of at least `index`, and returns
    /// that report. A restarted node may briefly serve an older prefix (C-4, C-7): its
    /// startup replay reaches only the persisted commit index (§7.7 step 5), and the rest
    /// applies once its new term commits.
    pub fn wait_applied(&self, index: u64) -> Reply {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let cluster = self.cluster();
            if cluster.body["applied_index"]
                .as_u64()
                .is_some_and(|applied| applied >= index)
            {
                return cluster;
            }
            assert!(
                Instant::now() < deadline,
                "fixture: node {} applies index {index} within 20 s of start {}; last \
                 report {}; stderr: {}",
                self.spec.node,
                self.starts,
                cluster.body,
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `/v1/cluster`, from this node: a fixture failure if whatever answers on its address
    /// is another cluster, so a test never asserts on a stranger's answers (br-p9fm).
    fn cluster(&self) -> Reply {
        let cluster = send(self.spec.address, &request("GET", "/v1/cluster", &[], ""));
        assert_eq!(
            cluster.body["cluster_id"].as_str(),
            Some(self.spec.cluster.as_str()),
            "fixture: {} answers as another cluster, not node {}'s: another test's server holds \
             its port (br-p9fm); answer {}; stderr: {}",
            self.spec.address,
            self.spec.node,
            cluster.body,
            self.stderr()
        );
        cluster
    }

    fn stderr_path(&self) -> PathBuf {
        self.spec
            .dir
            .join(format!("node-{}.stderr", self.spec.node))
    }

    fn spawn(&mut self) {
        let receipt = self.receipt();
        assert!(
            !receipt.exists(),
            "fixture: no receipt for node {} before a start: {}",
            self.spec.node,
            receipt.display()
        );
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.stderr_path())
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_keyval-server"))
            .args(["run", "--config"])
            .arg(&self.spec.config)
            .args(["--node", &self.spec.node.to_string(), "--log-file"])
            .arg(&self.spec.log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let record = json!({
            "node": self.spec.node,
            "pid": child.id(),
            "config": self.spec.config.display().to_string(),
            "address": self.spec.address.to_string(),
            "log": self.spec.log.display().to_string(),
        });
        self.child = Some(child);
        self.starts += 1;
        // Written whole, then renamed into place, so a reader never sees half a receipt.
        let partial = receipt.with_extension("partial");
        fs::write(&partial, record.to_string()).unwrap();
        fs::rename(&partial, &receipt).unwrap();
    }
}

impl Drop for Node {
    /// A test that fails mid-way leaves no process behind: the child is still unreaped, so
    /// its PID is still the one this helper started.
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(self.receipt());
        }
    }
}

/// The PID a receipt names.
fn read_receipt(path: &Path) -> Result<u64, Refusal> {
    let text = fs::read_to_string(path)
        .map_err(|err| Refusal::NoReceipt(format!("{}: {err}", path.display())))?;
    let record: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| Refusal::NoReceipt(format!("{}: {err}", path.display())))?;
    record["pid"]
        .as_u64()
        .ok_or_else(|| Refusal::NoReceipt(format!("{}: no pid in {text}", path.display())))
}

/// Whether `status` is a death by SIGKILL.
pub fn killed(status: ExitStatus) -> bool {
    status.signal() == Some(SIGKILL)
}

/// Sends `raw` to `address` and reads one response, or says why there is none: the
/// connection failed, or closed before a whole response (its server was killed).
pub fn try_send(address: SocketAddr, raw: &[u8]) -> Result<Reply, String> {
    let mut stream = TcpStream::connect(address).map_err(|err| format!("connect: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|err| format!("timeout: {err}"))?;
    stream
        .write_all(raw)
        .map_err(|err| format!("write: {err}"))?;
    try_read_reply(&mut BufReader::new(stream))
}
