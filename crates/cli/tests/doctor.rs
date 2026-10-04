//! `keyval doctor`'s tests through the real binary (PLAN.md O-6, O-7, W23.2): the verdict
//! on every recovery golden journal in a real data directory, the report's schema, and
//! that the directory is left byte for byte as it was.
#![cfg(test)]

// check: DOCTOR-CLI

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use fs_storage::{FsStorage, create_data_dir};
use journal::{FrameKind, Genesis, encode};
use serde_json::Value;
use types::{Config, DataDir, InstanceId, NodeId};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// The report's JSON Schema.
const SCHEMA: &str = include_str!("../../../schemas/cli/doctor.v1.schema.json");

/// A fresh path for a data directory under cargo's per-target scratch directory.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "doctor-{name}-{nanos}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The repository's `config/local.toml`, as a path for `--config`.
fn local_config_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/local.toml")
}

/// Node 1's genesis frame under `config/local.toml`.
fn genesis_frame() -> Vec<u8> {
    let config = Config::parse(LOCAL, Path::new("config")).unwrap();
    let instance = InstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
    let genesis = Genesis::new(&config, NodeId::new(1).unwrap(), instance).unwrap();
    encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap()
}

/// A data directory a node has used: created, locked once (so `LOCK` exists), with
/// `journal` as its journal file.
fn data_dir(name: &str, journal: &[u8]) -> PathBuf {
    let dir = scratch(name);
    create_data_dir(&dir).unwrap();
    let mut node = FsStorage::open(&dir).unwrap();
    node.lock().unwrap();
    drop(node);
    fs::write(dir.join(fs_storage::JOURNAL), journal).unwrap();
    dir
}

/// Every file in `dir` with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| {
                    let entry = entry.unwrap();
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        fs::read(entry.path()).unwrap(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Runs the `keyval` binary with `args`.
fn keyval(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_keyval"))
        .args(args)
        .output()
        .unwrap()
}

/// Runs `keyval doctor <dir> --json` plus `extra`, returning the report and exit code,
/// and checks the report against its schema.
fn doctor_json(dir: &Path, extra: &[&str]) -> (Value, i32) {
    let dir = dir.to_str().unwrap();
    let mut args = vec!["doctor", dir, "--json"];
    args.extend_from_slice(extra);
    let output = keyval(&args);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "rule: --json prints one JSON report (O-6); {err}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_schema(&report);
    (report, output.status.code().unwrap())
}

/// Asserts that `report` is a valid `keyval.cli.doctor/v1` object.
fn assert_schema(report: &Value) {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&schema)
        .unwrap();
    let errors: Vec<String> = validator
        .iter_errors(report)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "rule: every report validates against keyval.cli.doctor/v1 (O-6); errors={errors:?} \
         report={report}"
    );
}

/// A recovery golden: its file, whether it is a whole journal, and the expected verdict,
/// tail shape and refusal code.
struct Golden {
    /// The file name in `crates/journal/golden/recovery/`.
    name: &'static str,
    /// The file's bytes.
    bytes: &'static [u8],
    /// Whether it is a whole journal (no genesis prepended).
    whole: bool,
    /// The expected verdict.
    verdict: &'static str,
    /// The expected tail shape, or refusal code.
    detail: Option<&'static str>,
}

/// Every recovery golden with the doctor's expected verdict.
fn goldens() -> Vec<Golden> {
    macro_rules! golden {
        ($name:literal, $whole:expr, $verdict:literal, $detail:expr) => {
            Golden {
                name: $name,
                bytes: include_bytes!(concat!("../../journal/golden/recovery/", $name, ".bin")),
                whole: $whole,
                verdict: $verdict,
                detail: $detail,
            }
        };
    }
    vec![
        golden!("empty", true, "refuse", Some("empty")),
        golden!(
            "incomplete-genesis",
            true,
            "refuse",
            Some("incomplete_genesis")
        ),
        golden!("clean", false, "clean", None),
        golden!("logical-truncation", false, "clean", None),
        golden!("torn-mid-header", false, "discard_tail", Some("unreadable")),
        golden!(
            "torn-mid-payload",
            false,
            "discard_tail",
            Some("incomplete")
        ),
        golden!(
            "torn-mid-trailer",
            false,
            "discard_tail",
            Some("incomplete")
        ),
        golden!(
            "bad-checksum-final",
            false,
            "discard_tail",
            Some("checksum_failed")
        ),
        golden!("zero-tail", false, "discard_tail", Some("unreadable")),
        golden!("garbage-tail", false, "discard_tail", Some("unreadable")),
        golden!(
            "crash-during-truncation",
            false,
            "discard_tail",
            Some("unreadable")
        ),
        golden!("bad-checksum-mid", false, "refuse", Some("corruption")),
        golden!(
            "corrupt-header-then-valid",
            false,
            "refuse",
            Some("corruption")
        ),
        golden!("sequence-gap", false, "refuse", Some("corruption")),
        golden!("tail-too-long", false, "refuse", Some("tail_too_long")),
        golden!("truncation-below-commit", false, "refuse", Some("apply")),
        golden!("commit-regression", false, "refuse", Some("apply")),
    ]
}

/// A golden's journal bytes: whole, or its body after node 1's genesis.
fn journal_of(golden: &Golden) -> Vec<u8> {
    if golden.whole {
        golden.bytes.to_vec()
    } else {
        let mut bytes = genesis_frame();
        bytes.extend_from_slice(golden.bytes);
        bytes
    }
}

mod doctor_cli {
    use super::*;

    #[test]
    fn every_recovery_golden_gets_its_verdict_and_its_directory_is_untouched() {
        for golden in goldens() {
            let dir = data_dir(golden.name, &journal_of(&golden));
            let before = snapshot(&dir);
            let (report, code) = doctor_json(&dir, &[]);
            let name = golden.name;
            assert_eq!(
                report["verdict"], golden.verdict,
                "rule: the doctor reaches startup recovery's verdict (O-7); golden={name} \
                 report={report}"
            );
            let detail = match golden.verdict {
                "discard_tail" => report["tail"]["shape"].as_str(),
                "refuse" => report["refusal"]["code"].as_str(),
                _ => None,
            };
            assert_eq!(detail, golden.detail, "rule: golden={name} report={report}");
            assert_eq!(
                code,
                i32::from(golden.verdict == "refuse"),
                "rule: exit 0 if startup would start, 1 if it would refuse; golden={name}"
            );
            assert_eq!(
                (report["lock"].as_str(), report["binding"].as_str()),
                (Some("acquired"), Some("unchecked")),
                "rule: the lock was probed first and was free; no config, no binding check"
            );
            assert_eq!(
                snapshot(&dir),
                before,
                "rule: the doctor never modifies the directory (O-7); golden={name}"
            );
        }
    }

    #[test]
    fn a_live_directory_is_refused_unexamined() {
        let dir = data_dir("live", &genesis_frame());
        let mut node = FsStorage::open(&dir).unwrap();
        node.lock().unwrap();
        let before = snapshot(&dir);
        let (report, code) = doctor_json(&dir, &[]);
        assert_eq!(
            (
                report["verdict"].as_str(),
                report["lock"].as_str(),
                report["frames"].is_null(),
                code
            ),
            (Some("live"), Some("live"), true, 1),
            "rule: a locked directory is live and refused before anything is read (O-7); \
             report={report}"
        );
        assert_eq!(snapshot(&dir), before, "rule: nothing was modified");
        drop(node);
    }

    #[test]
    fn the_binding_is_verified_against_a_config() {
        let dir = data_dir("bound", &genesis_frame());
        let config = local_config_path();
        let config = config.to_str().unwrap();
        let (bound, code) = doctor_json(&dir, &["--config", config, "--node", "1"]);
        assert_eq!(
            (bound["verdict"].as_str(), bound["binding"].as_str(), code),
            (Some("clean"), Some("verified"), 0),
            "rule: with --config and --node the genesis binding is verified (DUR-8); \
             report={bound}"
        );
        let (other, code) = doctor_json(&dir, &["--config", config, "--node", "2"]);
        assert_eq!(
            (
                other["verdict"].as_str(),
                other["refusal"]["code"].as_str(),
                code
            ),
            (Some("refuse"), Some("genesis_mismatch"), 1),
            "rule: another node's directory is refused (DUR-8); report={other}"
        );
        assert_eq!(
            (other["binding"].as_str(), &other["genesis"]["node_id"]),
            (Some("mismatch"), &Value::from(1)),
            "rule: a mismatched binding is reported as such, with the genesis it read; \
             report={other}"
        );
        let corrupt = goldens()
            .into_iter()
            .find(|golden| golden.name == "bad-checksum-mid")
            .unwrap();
        let corrupt = data_dir("bound-corrupt", &journal_of(&corrupt));
        let (later, _) = doctor_json(&corrupt, &["--config", config, "--node", "1"]);
        assert_eq!(
            (later["refusal"]["code"].as_str(), later["binding"].as_str()),
            (Some("corruption"), Some("verified")),
            "rule: a refusal past the genesis record still verified the binding; \
             report={later}"
        );
        let empty = data_dir("bound-empty", &[]);
        let (unread, _) = doctor_json(&empty, &["--config", config, "--node", "1"]);
        assert_eq!(
            (
                unread["refusal"]["code"].as_str(),
                unread["binding"].as_str()
            ),
            (Some("empty"), Some("unchecked")),
            "rule: without a genesis record nothing was verified, --config or not; \
             report={unread}"
        );
    }

    #[test]
    fn blank_and_missing_directories_are_reported_and_left_alone() {
        let blank = scratch("blank");
        create_data_dir(&blank).unwrap();
        let (report, code) = doctor_json(&blank, &[]);
        assert_eq!(
            (report["verdict"].as_str(), report["lock"].as_str(), code),
            (Some("no_journal"), Some("no_lock_file"), 1),
            "rule: a blank directory never starts (D-52); report={report}"
        );
        assert!(
            snapshot(&blank).is_empty(),
            "rule: the doctor creates nothing, not even LOCK"
        );
        let missing = scratch("missing");
        let (report, code) = doctor_json(&missing, &[]);
        assert_eq!(
            (report["verdict"].as_str(), report["lock"].is_null(), code),
            (Some("unreadable"), true, 1),
            "rule: a missing directory cannot be examined; report={report}"
        );
        assert!(!missing.exists(), "rule: the doctor creates no directory");
    }

    #[test]
    fn the_human_report_says_what_recovery_would_do() {
        let torn = goldens()
            .into_iter()
            .find(|golden| golden.name == "torn-mid-payload")
            .unwrap();
        let dir = data_dir("human", &journal_of(&torn));
        let output = keyval(&["doctor", dir.to_str().unwrap()]);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("verdict: discard the 44-byte incomplete final frame (of 114 bytes)")
                && text.contains("then start"),
            "rule: the doctor says what startup recovery would do (O-7); output={text}"
        );
        assert_eq!(output.status.code(), Some(0), "rule: it would start");
    }

    #[test]
    fn usage_errors_exit_2() {
        for args in [
            vec!["doctor"],
            vec!["doctor", "dir", "--config", "config/local.toml"],
            vec!["doctor", "dir", "--node", "1"],
        ] {
            assert_eq!(
                keyval(&args).status.code(),
                Some(2),
                "rule: a usage error exits 2; args={args:?}"
            );
        }
        let unreadable_config = scratch("no-config");
        let output = keyval(&[
            "doctor",
            "dir",
            "--config",
            unreadable_config.to_str().unwrap(),
            "--node",
            "1",
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "rule: an unreadable --config is a usage error"
        );
    }

    #[test]
    fn the_schema_rejects_what_the_report_never_says() {
        let dir = data_dir("schema", &genesis_frame());
        let (mut report, _) = doctor_json(&dir, &[]);
        report["verdict"] = Value::from("maybe");
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        assert!(
            !validator.is_valid(&report),
            "rule: the schema is closed: an unknown verdict is invalid"
        );
    }
}
