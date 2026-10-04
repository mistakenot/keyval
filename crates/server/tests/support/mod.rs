//! Helpers both test targets share: scratch directories under cargo's per-target temp
//! dir, and configs written into them whose relative data directories resolve there.
#![cfg(test)]

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use journal::{FrameKind, Genesis, encode};
use types::{Config, InstanceId, NodeId};

const ONE_VOTER: &str = include_str!("../../../../config/fixtures/one-voter.toml");

/// The instance ID the tests stamp.
pub const INSTANCE: &str = "00112233445566778899aabbccddeeff";

pub fn instance() -> InstanceId {
    InstanceId::parse(INSTANCE).unwrap()
}

pub fn node(id: u64) -> NodeId {
    NodeId::new(id).unwrap()
}

/// A fresh scratch directory: the config's directory, holding no data directory yet.
pub fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "server-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A one-voter config (node 1) in `dir`, whose relative data directory `node-1` resolves
/// against `dir`, with `edit` applied to its text. Returns the file's path and the config.
pub fn one_voter(dir: &Path, edit: impl Fn(String) -> String) -> (PathBuf, Config) {
    let text = edit(ONE_VOTER.replace("nodes/one-voter/1", "node-1"));
    write_config(dir, "one-voter.toml", &text)
}

/// Writes `text` to `dir/name` and parses it as a config file in `dir`.
pub fn write_config(dir: &Path, name: &str, text: &str) -> (PathBuf, Config) {
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    let config = Config::parse(text, dir).unwrap();
    (path, config)
}

/// The data directory of `id` in `config`.
pub fn data_dir(config: &Config, id: u64) -> PathBuf {
    config.node(node(id)).unwrap().data_dir().to_path_buf()
}

/// The journal's bytes, or `None` without one.
pub fn journal_bytes(dir: &Path) -> Option<Vec<u8>> {
    fs::read(dir.join(fs_storage::JOURNAL)).ok()
}

/// The genesis frame `init` writes for `id` in `config`.
pub fn genesis_frame(config: &Config, id: u64) -> Vec<u8> {
    let genesis = Genesis::new(config, node(id), instance()).unwrap();
    encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap()
}

/// A free loopback port, for a node's client address in a config.
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What an HTTP call got back.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

impl Reply {
    /// Every value of the header `name` (lower-case).
    pub fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// The error envelope's code, if the body is one.
    pub fn code(&self) -> Option<&str> {
        self.body["error"]["code"].as_str()
    }
}

/// Sends `raw` (a whole HTTP/1.1 request) to `address` and reads one response.
pub fn send(address: SocketAddr, raw: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream.write_all(raw).unwrap();
    read_reply(&mut BufReader::new(stream))
}

/// Reads one response from `reader`.
pub fn read_reply(reader: &mut impl BufRead) -> Reply {
    try_read_reply(reader).unwrap_or_else(|why| panic!("fixture: a whole HTTP response ({why})"))
}

/// Reads one response from `reader`, or says why there is none.
pub fn try_read_reply(reader: &mut impl BufRead) -> Result<Reply, String> {
    let line = |reader: &mut dyn BufRead| -> Result<String, String> {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => Err("closed before a whole response".to_owned()),
            Ok(_) => Ok(line),
            Err(err) => Err(format!("read: {err}")),
        }
    };
    let status_line = line(reader)?;
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| format!("status line {status_line:?}"))?;
    let mut headers = Vec::new();
    loop {
        let header = line(reader)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| format!("header {header:?}"))?;
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map_or(Ok(0), |(_, value)| value.parse())
        .map_err(|err| format!("content-length: {err}"))?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|err| format!("body: {err}"))?;
    Ok(Reply {
        status,
        headers,
        body: serde_json::from_slice(&body).map_err(|err| format!("body JSON: {err}"))?,
    })
}

/// A request to `target` with `method`, the extra `headers`, and `body` (with its
/// Content-Length), closing the connection after the response.
pub fn request(method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        let _ = write!(raw, "{name}: {value}\r\n");
    }
    if !body.is_empty() {
        let _ = write!(raw, "Content-Length: {}\r\n", body.len());
    }
    raw.push_str("\r\n");
    raw.push_str(body);
    raw.into_bytes()
}

/// The JSON body of a PUT of `value`.
pub fn put_body(value: &str) -> String {
    serde_json::json!({"value": value}).to_string()
}
