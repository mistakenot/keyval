//! The `keyval` client commands as a person runs them (W33.3): the real binary, real
//! loopback sockets, fake nodes that script the answers, and the output validated against
//! each command's versioned schema (§8.7). Test evidence over fake nodes, not proof.

#![cfg(test)]

mod client_cli {
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};

    const WRITE: &str = include_str!("../../../schemas/cli/write.v1.schema.json");
    const GET: &str = include_str!("../../../schemas/cli/get.v1.schema.json");
    const SCAN: &str = include_str!("../../../schemas/cli/scan.v1.schema.json");
    const STATUS: &str = include_str!("../../../schemas/cli/status.v1.schema.json");
    const CLUSTER: &str = include_str!("../../../schemas/cli/cluster.v1.schema.json");
    const PENDING: &str = include_str!("../../../schemas/cli/pending-list.v1.schema.json");
    const ERROR: &str = include_str!("../../../schemas/cli/error.v1.schema.json");
    const LOCAL_CONFIG: &str = include_str!("../../../config/local.toml");

    /// What a fake node saw.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Seen {
        node: u64,
        method: String,
        target: String,
        idempotency_key: Option<String>,
        body: String,
    }

    /// What a fake node answers.
    enum Reply {
        Json(u16, Value),
        /// Read the request, then close without answering.
        Close,
    }

    type Handler = dyn Fn(usize, &Seen) -> Reply + Send + Sync;

    /// Five fake nodes on loopback, all answering through one handler.
    struct Cluster {
        addrs: Vec<SocketAddr>,
        log: Arc<Mutex<Vec<Seen>>>,
        handler: Arc<Mutex<Arc<Handler>>>,
    }

    impl Cluster {
        fn start(handler: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static) -> Self {
            let log = Arc::new(Mutex::new(Vec::new()));
            let handler: Arc<Mutex<Arc<Handler>>> = Arc::new(Mutex::new(Arc::new(handler)));
            let mut addrs = Vec::new();
            for node in 1..=5_u64 {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                addrs.push(listener.local_addr().unwrap());
                let (log, handler) = (Arc::clone(&log), Arc::clone(&handler));
                thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        serve(stream, node, &log, &handler);
                    }
                });
            }
            Self {
                addrs,
                log,
                handler,
            }
        }

        fn set(&self, handler: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static) {
            *self.handler.lock().unwrap() = Arc::new(handler);
        }

        fn seen(&self) -> Vec<Seen> {
            self.log.lock().unwrap().clone()
        }
    }

    fn serve(
        mut stream: TcpStream,
        node: u64,
        log: &Mutex<Vec<Seen>>,
        handler: &Mutex<Arc<Handler>>,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut data = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (head_end, length) = loop {
            let Ok(read) = stream.read(&mut chunk) else {
                return;
            };
            if read == 0 {
                return;
            }
            data.extend_from_slice(&chunk[..read]);
            if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..at]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (at + 4, length);
            }
        };
        while data.len() < head_end + length {
            let Ok(read) = stream.read(&mut chunk) else {
                return;
            };
            if read == 0 {
                return;
            }
            data.extend_from_slice(&chunk[..read]);
        }
        let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
        let mut lines = head.lines();
        let mut first = lines.next().unwrap_or("").split(' ');
        let method = first.next().unwrap_or("").to_owned();
        let target = first.next().unwrap_or("").to_owned();
        let idempotency_key = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("idempotency-key")
                .then(|| value.trim().to_owned())
        });
        let seen = Seen {
            node,
            method,
            target,
            idempotency_key,
            body: String::from_utf8_lossy(&data[head_end..head_end + length]).into_owned(),
        };
        let seq = {
            let mut log = log.lock().unwrap();
            log.push(seen.clone());
            log.len() - 1
        };
        let current = Arc::clone(&handler.lock().unwrap());
        match current(seq, &seen) {
            Reply::Close => {}
            Reply::Json(status, body) => {
                let body = body.to_string();
                let reason = if status == 200 { "OK" } else { "Status" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "client-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The local config with the nodes' client addresses replaced by the fakes', and a short
    /// client deadline so that an unresolved run is quick.
    fn config(dir: &Path, cluster: &Cluster) -> PathBuf {
        let mut text = LOCAL_CONFIG.to_owned();
        for (at, addr) in cluster.addrs.iter().enumerate() {
            let old = format!("client_address = \"127.0.0.1:700{}\"", at + 1);
            assert!(text.contains(&old), "the template has {old}");
            text = text.replace(&old, &format!("client_address = \"{addr}\""));
        }
        for (old, new) in [
            ("tick_ms = 50", "tick_ms = 5"),
            ("write_deadline_ticks = 100", "write_deadline_ticks = 40"),
            ("cli_deadline_ms = 15000", "cli_deadline_ms = 800"),
        ] {
            assert!(text.contains(old), "the template has {old}");
            text = text.replace(old, new);
        }
        let path = dir.join("test.toml");
        fs::write(&path, text).unwrap();
        path
    }

    struct Rig {
        cluster: Cluster,
        config: PathBuf,
        pending: PathBuf,
    }

    impl Rig {
        fn new(
            name: &str,
            handler: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static,
        ) -> Self {
            let dir = scratch(name);
            let cluster = Cluster::start(handler);
            let config = config(&dir, &cluster);
            Self {
                cluster,
                config,
                pending: dir.join("pending"),
            }
        }

        /// Runs `keyval <args> --config … --pending-dir … --json`.
        fn run(&self, args: &[&str]) -> (Value, i32, Output) {
            let output = Command::new(env!("CARGO_BIN_EXE_keyval"))
                .args(args)
                .arg("--config")
                .arg(&self.config)
                .arg("--pending-dir")
                .arg(&self.pending)
                .arg("--json")
                .output()
                .unwrap();
            let doc: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
                panic!(
                    "rule: --json prints one JSON document (O-6); {err}; args={args:?}; \
                     stdout={}; stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            let code = output.status.code().unwrap();
            (doc, code, output)
        }

        fn pending_ids(&self) -> Vec<String> {
            let (doc, _, _) = self.run(&["pending"]);
            validate(PENDING, &doc);
            doc["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["request_id"].as_str().unwrap().to_owned())
                .collect()
        }
    }

    fn validate(schema: &str, doc: &Value) {
        let schema: Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        let errors: Vec<String> = validator
            .iter_errors(doc)
            .map(|e| format!("{e} at {}", e.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "rule: output validates against its schema (§8.7); errors={errors:?}; doc={doc}"
        );
    }

    fn served(node: u64, applied: u64) -> Value {
        json!({"node_id": node, "applied_index": applied})
    }

    fn put_ok(index: u64) -> Value {
        json!({"request_id": "x", "index": index, "version": index, "replayed": false,
            "served_by": served(1, index)})
    }

    fn error(code: &str, details: &Value) -> Value {
        json!({"error": {"code": code, "message": "scripted", "details": details}})
    }

    // check: CLI-CLIENT
    #[test]
    fn put_prints_a_valid_report_and_leaves_nothing_pending() {
        let rig = Rig::new("put", |_, _| Reply::Json(200, put_ok(9)));
        let (doc, code, output) = rig.run(&["put", "greeting", "hello"]);
        validate(WRITE, &doc);
        assert_eq!(
            code,
            0,
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(doc["result"], "applied");
        let seen = rig.cluster.seen();
        assert_eq!(seen.len(), 1, "seen={seen:?}");
        assert_eq!(seen[0].method, "PUT");
        assert_eq!(seen[0].target, "/v1/kv?key=greeting");
        assert_eq!(seen[0].body, r#"{"value":"hello"}"#);
        let id = seen[0].idempotency_key.clone().unwrap();
        assert_eq!(id.len(), 32, "a 32-character request ID; id={id}");
        assert_eq!(doc["request_id"], id.as_str());
        assert_eq!(rig.pending_ids(), Vec::<String>::new());
    }

    #[test]
    fn a_redirect_keeps_the_id_and_the_command() {
        let rig = Rig::new("redirect", |seq, seen| {
            if seq == 0 {
                Reply::Json(
                    307,
                    error(
                        "not_leader",
                        &json!({"leader_hint": seen.node % 5 + 1, "request_id": "x",
                            "attempt_state": "not_submitted"}),
                    ),
                )
            } else {
                Reply::Json(200, put_ok(12))
            }
        });
        let (doc, code, _) = rig.run(&["put", "k", "v"]);
        validate(WRITE, &doc);
        assert_eq!(code, 0, "doc={doc}");
        let seen = rig.cluster.seen();
        assert_eq!(seen.len(), 2, "seen={seen:?}");
        assert_ne!(
            seen[0].node, seen[1].node,
            "the second send followed the hint"
        );
        assert_eq!(
            (
                &seen[0].method,
                &seen[0].target,
                &seen[0].idempotency_key,
                &seen[0].body
            ),
            (
                &seen[1].method,
                &seen[1].target,
                &seen[1].idempotency_key,
                &seen[1].body
            ),
            "rule: a redirect resends the same ID and bytes (C-6)"
        );
    }

    #[test]
    fn a_terminal_mismatch_exits_one_and_is_not_unresolved() {
        let rig = Rig::new("mismatch", |_, _| {
            Reply::Json(
                409,
                error(
                    "version_mismatch",
                    &json!({"current_version": 57, "request_id": "x", "index": 58,
                        "replayed": false}),
                ),
            )
        });
        let (doc, code, _) = rig.run(&["cas", "k", "v", "--version", "41"]);
        validate(WRITE, &doc);
        assert_eq!(code, 1);
        assert_eq!(doc["result"], "version_mismatch");
        assert_eq!(doc["unresolved"], Value::Null);
        assert_eq!(rig.pending_ids(), Vec::<String>::new());
        let seen = rig.cluster.seen();
        assert_eq!(seen[0].target, "/v1/cas?key=k");
    }

    #[test]
    fn silence_is_unresolved_keeps_the_record_and_resume_finishes_it_with_the_same_id() {
        let rig = Rig::new("silence", |_, _| Reply::Close);
        let (doc, code, _) = rig.run(&["put", "k", "v"]);
        validate(WRITE, &doc);
        assert_eq!(code, 3, "rule: unresolved exits 3; doc={doc}");
        assert_eq!(doc["result"], "unresolved");
        let id = doc["request_id"].as_str().unwrap().to_owned();
        assert_eq!(doc["resume"], format!("keyval resume {id}"));
        let human = Command::new(env!("CARGO_BIN_EXE_keyval"))
            .args(["put", "k", "v", "--config"])
            .arg(&rig.config)
            .arg("--pending-dir")
            .arg(&rig.pending)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&human.stdout).into_owned();
        assert!(
            text.contains("UNRESOLVED") && !text.to_lowercase().contains("failed"),
            "rule: unresolved, never failed; text={text}; stderr={}",
            String::from_utf8_lossy(&human.stderr)
        );
        assert_eq!(human.status.code(), Some(3));
        let ids = rig.pending_ids();
        assert_eq!(ids.len(), 2, "the two puts are both pending; ids={ids:?}");
        assert!(ids.contains(&id));

        let first_sends: Vec<Seen> = rig.cluster.seen();
        rig.cluster.set(|_, _| Reply::Json(200, put_ok(5)));
        let (resumed, code, _) = rig.run(&["resume", &id]);
        validate(WRITE, &resumed);
        assert_eq!(code, 0, "resumed={resumed}");
        assert_eq!(resumed["command"], "resume");
        assert_eq!(resumed["request_id"], id.as_str());
        let last = rig.cluster.seen().pop().unwrap();
        let original = first_sends
            .iter()
            .find(|s| s.idempotency_key.as_deref() == Some(id.as_str()))
            .unwrap();
        assert_eq!(
            (&last.method, &last.target, &last.body),
            (&original.method, &original.target, &original.body),
            "rule: resume sends the exact original command (O-4)"
        );
        assert_eq!(last.idempotency_key.as_deref(), Some(id.as_str()));
        let left = rig.pending_ids();
        assert_eq!(left.len(), 1, "only the other put remains; left={left:?}");
        assert!(!left.contains(&id), "the resumed record is deleted");
    }

    #[test]
    fn resume_that_runs_out_again_allocates_no_replacement_id() {
        let rig = Rig::new("again", |_, _| Reply::Close);
        let (first, code, _) = rig.run(&["delete", "k"]);
        assert_eq!(code, 3);
        let id = first["request_id"].as_str().unwrap().to_owned();
        for round in 0..2 {
            let (doc, code, _) = rig.run(&["resume", &id]);
            validate(WRITE, &doc);
            assert_eq!(code, 3, "round {round}; doc={doc}");
            assert_eq!(doc["request_id"], id.as_str());
            assert_eq!(doc["result"], "unresolved");
        }
        assert_eq!(
            rig.pending_ids(),
            vec![id.clone()],
            "one record, the original"
        );
        let keys: BTreeSet<String> = rig
            .cluster
            .seen()
            .into_iter()
            .filter_map(|s| s.idempotency_key)
            .collect();
        assert_eq!(keys, BTreeSet::from([id]), "every send used the one ID");
    }

    #[test]
    fn resume_of_an_unknown_id_prints_the_error_envelope() {
        let rig = Rig::new("nosuch", |_, _| Reply::Json(200, put_ok(1)));
        let (doc, code, _) = rig.run(&["resume", "0123456789abcdef0123456789abcdef"]);
        validate(ERROR, &doc);
        assert_eq!(code, 2);
        assert_eq!(doc["error"]["kind"], "pending");
        assert_eq!(rig.cluster.seen(), vec![], "nothing was sent");
    }

    #[test]
    fn reads_status_and_cluster_validate_against_their_schemas() {
        let rig = Rig::new("reads", |_, seen| {
            if seen.target.starts_with("/v1/kv") {
                Reply::Json(
                    200,
                    json!({"key": "a", "value": "hi", "version": 3, "served_by": served(1, 9)}),
                )
            } else if seen.target.starts_with("/v1/scan") {
                Reply::Json(
                    200,
                    json!({"items": [{"key": "a", "value": "hi", "version": 3}],
                        "has_more": false, "next_after": null, "served_by": served(1, 9)}),
                )
            } else if seen.target.starts_with("/v1/requests/") {
                Reply::Json(200, json!({"state": "unknown", "served_by": served(1, 2)}))
            } else {
                Reply::Json(200, json!({"node_id": 1, "role": "leader"}))
            }
        });
        let (get, code, _) = rig.run(&["get", "a"]);
        validate(GET, &get);
        assert_eq!((code, get["result"].as_str()), (0, Some("found")));
        let (scan, code, _) = rig.run(&["scan", "--limit", "5"]);
        validate(SCAN, &scan);
        assert_eq!((code, scan["count"].as_u64()), (0, Some(1)));
        let (status, code, _) = rig.run(&["status", "0123456789abcdef0123456789abcdef"]);
        validate(STATUS, &status);
        assert_eq!(
            (code, status["state"].as_str()),
            (3, Some("unknown")),
            "rule: unknown is unresolved, not failed"
        );
        let (cluster, code, _) = rig.run(&["cluster"]);
        validate(CLUSTER, &cluster);
        assert_eq!(code, 0);
    }

    #[test]
    fn a_usage_error_under_json_prints_the_error_envelope() {
        let rig = Rig::new("usage", |_, _| Reply::Json(200, put_ok(1)));
        let (doc, code, _) = rig.run(&["get", ""]);
        validate(ERROR, &doc);
        assert_eq!(code, 2);
        assert_eq!(doc["error"]["kind"], "usage", "doc={doc}");
        assert_eq!(rig.cluster.seen(), vec![]);
    }
}
