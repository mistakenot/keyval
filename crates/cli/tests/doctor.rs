//! `keyval doctor`'s tests through the real binary (PLAN.md O-6, O-7, W23.2): the verdict
//! on every recovery golden journal in a real data directory, the report's schema, and
//! that the directory is left byte for byte as it was.
#![cfg(test)]

// check: DOCTOR-CLI

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use fs_storage::{DataDirCreated, FsStorage, create_data_dir};
use journal::{FrameKind, Genesis, LogBounds, TailShape, Verdict, encode, examine};
use serde_json::{Value, json};
use types::{
    ByteCount, Config, DataDir, InstanceId, JournalRead, LogIndex, NodeId, ReadLen, StorageError,
    Term,
};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// The report's JSON Schema (v2 since br-riea).
const SCHEMA: &str = include_str!("../../../schemas/cli/doctor.v2.schema.json");

/// The error envelope's JSON Schema (br-tdy6).
const ERROR_SCHEMA: &str = include_str!("../../../schemas/cli/error.v1.schema.json");

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

/// A data directory a node has used: created, with the empty `LOCK` file a node's lock
/// leaves behind, and `journal` as its journal file.
///
/// The test never takes the lock itself. A process that a sibling test spawns while the
/// lock is held inherits it until it execs, so this test's doctor could see a live node
/// and judge nothing (br-o8co).
fn data_dir(name: &str, journal: &[u8]) -> PathBuf {
    let dir = scratch(name);
    assert_eq!(create_data_dir(&dir), Ok(DataDirCreated::New));
    fs::write(dir.join(fs_storage::LOCK), b"").unwrap();
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
        golden!(
            "impossible-version-final",
            false,
            "refuse",
            Some("impossible_header")
        ),
    ]
}

/// Every variant of a report enum, serialised as the report writes it. The variants are
/// listed once, for both the list and an exhaustive match, so a new variant fails to compile
/// until it is listed here (br-ozvx).
macro_rules! every {
    ($ty:ident: $($variant:ident),+ $(,)?) => {{
        let _exhaustive = |value: $ty| match value {
            $($ty::$variant => ()),+
        };
        [$($ty::$variant),+]
            .iter()
            .map(|value| serde_json::to_value(value).unwrap().to_string())
            .collect::<BTreeSet<String>>()
    }};
}

/// The logical log's bounds the CLI uses without a configuration (`doctor::UNCONFIGURED`).
const UNCONFIGURED: LogBounds = LogBounds::new(ByteCount::new(8 << 20), ByteCount::new(u64::MAX));

/// A journal held in memory, for examining the same bytes the CLI reads.
struct Memory(Vec<u8>);

impl JournalRead for Memory {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        let start = usize::try_from(offset.get()).unwrap().min(self.0.len());
        let end = (start + usize::try_from(len.get()).unwrap()).min(self.0.len());
        if end - start < usize::try_from(len.get()).unwrap() {
            return Err(StorageError::ShortRead {
                wanted: len,
                got: u32::try_from(end - start).unwrap(),
            });
        }
        Ok(self.0[start..end].to_vec())
    }
}

/// The report's numbers that `journal::examine` gives for `bytes`, as the report would
/// write them (no configuration).
fn examined_numbers(bytes: &[u8]) -> Value {
    let len = ByteCount::new(u64::try_from(bytes.len()).unwrap());
    let examination = examine(&Memory(bytes.to_vec()), len, None, UNCONFIGURED);
    let tail = match examination.verdict() {
        Verdict::DiscardTail(tail) => Some(*tail),
        Verdict::Clean | Verdict::Refuse(_) => None,
    };
    json!({
        "journal_len": examination.journal_len().get(),
        "durable_len": examination.durable_len().map(ByteCount::get),
        "frames": examination.frames(),
        "last_index": examination.last_index().map(LogIndex::get),
        "last_term": examination.last_term().map(Term::get),
        "hard_state": examination.hard_state().map(|state| json!({
            "term": state.term.get(),
            "vote": state.vote.map(NodeId::get),
            "commit": state.commit.get(),
        })),
        "tail": tail.map(|tail| {
            let (frame_len, sequence) = match tail.shape() {
                TailShape::Incomplete { frame_len } => (Some(frame_len), None),
                TailShape::Stale { sequence } => (None, Some(sequence)),
                TailShape::ChecksumFailed | TailShape::Unreadable => (None, None),
            };
            json!({
                "start": tail.start().get(),
                "len": tail.len().get(),
                "frame_len": frame_len,
                "sequence": sequence,
            })
        }),
    })
}

/// The same numbers from `report` (the tail without its shape, which the verdict tests own).
fn reported_numbers(report: &Value) -> Value {
    let tail = &report["tail"];
    json!({
        "journal_len": report["journal_len"],
        "durable_len": report["durable_len"],
        "frames": report["frames"],
        "last_index": report["last_index"],
        "last_term": report["last_term"],
        "hard_state": report["hard_state"],
        "tail": if tail.is_null() { Value::Null } else { json!({
            "start": tail["start"],
            "len": tail["len"],
            "frame_len": tail["frame_len"],
            "sequence": tail["sequence"],
        }) },
    })
}

/// The golden named `name`.
fn golden_named(name: &str) -> Golden {
    goldens()
        .into_iter()
        .find(|golden| golden.name == name)
        .unwrap()
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
        assert_eq!(create_data_dir(&blank), Ok(DataDirCreated::New));
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
    fn the_report_gives_the_numbers_recovery_read() {
        for golden in goldens() {
            let bytes = journal_of(&golden);
            let (report, _) = doctor_json(&data_dir(golden.name, &bytes), &[]);
            assert_eq!(
                reported_numbers(&report),
                examined_numbers(&bytes),
                "rule: the report gives exactly the numbers recovery read (O-7); golden={}, \
                 report={report}",
                golden.name
            );
        }
        // Pinned from the goldens' README (F2 113 bytes, F3 87, F4 114) and the 147-byte
        // genesis of node 1 under config/local.toml.
        let genesis = u64::try_from(genesis_frame().len()).unwrap();
        assert_eq!(genesis, 147, "fixture: the genesis frame the pins assume");
        let torn = journal_of(&golden_named("torn-mid-payload"));
        let (report, _) = doctor_json(&data_dir("numbers-torn", &torn), &[]);
        assert_eq!(
            (
                &report["tail"],
                &report["durable_len"],
                &report["journal_len"]
            ),
            (
                &json!({"start": 347, "len": 44, "shape": "incomplete", "frame_len": 114, "sequence": null}),
                &json!(347),
                &json!(391)
            ),
            "rule: the tail is the 44 bytes of F4 after the durable 347 (O-7); report={report}"
        );
        let clean = journal_of(&golden_named("clean"));
        let (report, _) = doctor_json(&data_dir("numbers-clean", &clean), &[]);
        assert_eq!(
            (
                &report["frames"],
                &report["last_index"],
                &report["last_term"],
                &report["hard_state"]
            ),
            (
                &json!(3),
                &json!(3),
                &json!(1),
                &json!({"term": 1, "vote": 1, "commit": 2})
            ),
            "rule: clean.bin holds the genesis, F2 and F3: entries 1-3 in term 1, hard state \
             term 1, vote 1, commit 2; report={report}"
        );
        let mut stale = clean;
        stale.extend_from_slice(&golden_named("clean").bytes[..113]);
        let (report, _) = doctor_json(&data_dir("numbers-stale", &stale), &[]);
        assert_eq!(
            &report["tail"],
            &json!({"start": 347, "len": 113, "shape": "stale", "frame_len": null, "sequence": 2}),
            "rule: a stale copy of F2 after clean.bin is reported with its sequence; \
             report={report}"
        );
        let corrupt = journal_of(&golden_named("bad-checksum-mid"));
        let (report, _) = doctor_json(&data_dir("numbers-refused", &corrupt), &[]);
        assert_eq!(
            (
                &report["durable_len"],
                &report["frames"],
                &report["last_index"]
            ),
            (&json!(147), &json!(1), &json!(0)),
            "rule: a refusal at F2 reports the genesis read before it; report={report}"
        );
    }

    #[test]
    fn fields_never_read_are_null_not_zero() {
        let (empty, _) = doctor_json(&data_dir("unread-empty", &[]), &[]);
        assert_eq!(
            (
                &empty["frames"],
                &empty["durable_len"],
                &empty["last_index"],
                &empty["last_term"],
                &empty["hard_state"]
            ),
            (
                &json!(0),
                &Value::Null,
                &Value::Null,
                &Value::Null,
                &Value::Null
            ),
            "rule: with no genesis read, nothing reads as an empty log (br-ljgh); report={empty}"
        );
        let dir = data_dir("unread-mismatch", &genesis_frame());
        let config = local_config_path();
        let args = ["--config", config.to_str().unwrap(), "--node", "2"];
        let (other, _) = doctor_json(&dir, &args);
        assert_eq!(
            (
                &other["frames"],
                &other["durable_len"],
                &other["last_index"],
                &other["binding"]
            ),
            (&json!(1), &Value::Null, &Value::Null, &json!("mismatch")),
            "rule: a refused genesis was read, but no log was built, so nothing reads as \
             byte 0 or index 0 (br-ljgh); report={other}"
        );
        let output = keyval(&[
            "doctor",
            dir.to_str().unwrap(),
            args[0],
            args[1],
            args[2],
            args[3],
        ]);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("1 frames read")
                && !text.contains("through byte")
                && !text.contains("last index"),
            "rule: the text leaves out what was never read, never 0; output={text}"
        );
    }

    #[test]
    fn without_a_config_init_is_never_advised_past_any_genesis_frame() {
        let incomplete = journal_of(&golden_named("incomplete-genesis"));
        let mut damaged = journal_of(&golden_named("clean"));
        for at in [0, 147, 147 + 113] {
            damaged[at] ^= 0xff;
        }
        let config = local_config_path();
        let bound = ["--config", config.to_str().unwrap(), "--node", "1"];
        for (name, bytes, unbound, configured) in [
            (
                "4096 zero bytes",
                vec![0_u8; 4096],
                ("not_an_interrupted_init", json!(false)),
                ("not_an_interrupted_init", json!(false)),
            ),
            (
                "a journal whose frames all lost their first byte",
                damaged,
                ("incomplete_genesis", Value::Null),
                ("not_an_interrupted_init", json!(false)),
            ),
            (
                "347 zero bytes",
                vec![0_u8; 347],
                ("incomplete_genesis", Value::Null),
                ("not_an_interrupted_init", json!(false)),
            ),
            (
                "the 60-byte incomplete genesis",
                incomplete,
                ("incomplete_genesis", json!(true)),
                ("incomplete_genesis", json!(true)),
            ),
        ] {
            let dir = data_dir("init-advice", &bytes);
            for (args, (code, init)) in [(&[][..], unbound), (&bound[..], configured)] {
                let (report, _) = doctor_json(&dir, args);
                assert_eq!(
                    (
                        report["refusal"]["code"].as_str(),
                        &report["refusal"]["init_allowed"]
                    ),
                    (Some(code), &init),
                    "rule: init is advised only where no configuration could change the answer; \
                     never on a journal longer than any genesis frame (§7.7 step 0, br-riea); \
                     journal={name} args={args:?} report={report}"
                );
                let text = String::from_utf8_lossy(
                    &keyval(&[&["doctor", dir.to_str().unwrap()][..], args].concat()).stdout,
                )
                .into_owned();
                let advised = text.contains("`init` may (re-)run");
                let unknown = text.contains("run the doctor again with --config and --node");
                assert_eq!(
                    (advised, unknown),
                    (init == json!(true), init.is_null()),
                    "rule: the text says the same as init_allowed; journal={name} \
                     args={args:?} output={text}"
                );
            }
        }
    }

    #[test]
    fn a_journal_read_without_a_lock_file_is_flagged() {
        let dir = scratch("no-lock");
        assert_eq!(create_data_dir(&dir), Ok(DataDirCreated::New));
        fs::write(
            dir.join(fs_storage::JOURNAL),
            journal_of(&golden_named("clean")),
        )
        .unwrap();
        let (report, code) = doctor_json(&dir, &[]);
        assert_eq!(
            (report["verdict"].as_str(), report["lock"].as_str(), code),
            (Some("clean"), Some("no_lock_file"), 0),
            "rule: an unchanged journal read without a LOCK file keeps its verdict; \
             report={report}"
        );
        assert!(
            report["would_do"].as_str().is_some_and(
                |text| text.contains("no LOCK file") && text.contains("did not change")
            ),
            "rule: the report flags that nothing kept a node from starting (br-riea); \
             report={report}"
        );
    }

    #[test]
    fn a_journal_that_cannot_be_read_is_unreadable_with_its_cause() {
        let dir = data_dir("eisdir", &[]);
        fs::remove_file(dir.join(fs_storage::JOURNAL)).unwrap();
        fs::create_dir(dir.join(fs_storage::JOURNAL)).unwrap();
        let (report, code) = doctor_json(&dir, &[]);
        let would_do = report["would_do"].as_str().unwrap_or_default().to_owned();
        assert_eq!(
            (
                report["verdict"].as_str(),
                report["refusal"].is_null(),
                code
            ),
            (Some("unreadable"), true, 1),
            "rule: a journal that cannot be read is unreadable, not a refusal (br-riea); \
             report={report}"
        );
        assert!(
            would_do.contains("journal.log") && would_do.contains("directory"),
            "rule: the report keeps the operation, path and OS cause; would_do={would_do}"
        );
    }

    #[test]
    fn the_golden_table_is_every_recovery_golden_file() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../journal/golden/recovery");
        let files: BTreeSet<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "bin"))
            .collect();
        let table: BTreeSet<String> = goldens()
            .iter()
            .map(|golden| format!("{}.bin", golden.name))
            .collect();
        let readme = fs::read_to_string(dir.join("README.md")).unwrap();
        let rows: BTreeSet<String> = readme
            .lines()
            .filter_map(|line| line.strip_prefix("| `")?.split_once("` |"))
            .map(|(name, _)| name.to_owned())
            .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "bin"))
            .collect();
        assert_eq!(
            table, files,
            "rule: the doctor's verdict is checked on every recovery golden in the directory \
             (O-7, §12.2 W23); a new golden fails here until it is added (br-ozvx)"
        );
        assert_eq!(
            rows, files,
            "rule: the README indexes exactly the golden files, so the journal crate's own \
             tables, held to the README, cover them too"
        );
    }

    #[test]
    fn the_schemas_enums_are_exactly_the_reports_enums() {
        use cli::doctor::{BindingState, Conclusion, LockState, RefusalCode, ShapeReport};
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let listed = |pointer: &str| -> BTreeSet<String> {
            schema
                .pointer(pointer)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("the schema has no enum at {pointer}"))
                .iter()
                .filter(|value| !value.is_null())
                .map(Value::to_string)
                .collect()
        };
        let enums = [
            (
                "/properties/verdict/enum",
                every!(Conclusion: Clean, DiscardTail, Refuse, Live, NoJournal, Unreadable, Changed),
            ),
            (
                "/properties/lock/enum",
                every!(LockState: Acquired, NoLockFile, Live),
            ),
            (
                "/properties/binding/enum",
                every!(BindingState: Verified, Mismatch, Unchecked),
            ),
            (
                "/properties/tail/oneOf/1/properties/shape/enum",
                every!(ShapeReport: Incomplete, ChecksumFailed, Unreadable, Stale),
            ),
            (
                "/properties/refusal/oneOf/1/properties/code/enum",
                every!(RefusalCode:
                    Empty,
                    IncompleteGenesis,
                    NotAnInterruptedInit,
                    ImpossibleHeader,
                    NotGenesis,
                    GenesisRecord,
                    GenesisMismatch,
                    Record,
                    Apply,
                    Corruption,
                    TailTooLong,
                    Storage,
                ),
            ),
        ];
        for (pointer, variants) in enums {
            assert_eq!(
                listed(pointer),
                variants,
                "rule: the schema's enum lists exactly what the report serialises (O-6); \
                 pointer={pointer} (br-ozvx)"
            );
        }
        assert!(
            schema["properties"]["lock"]["enum"]
                .as_array()
                .is_some_and(|values| values.contains(&Value::Null)),
            "rule: lock is null when the directory cannot be opened"
        );
    }

    #[test]
    fn usage_and_config_errors_under_json_print_the_error_envelope() {
        use cli::output::ErrorKind;
        let schema: Value = serde_json::from_str(ERROR_SCHEMA).unwrap();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        let local = local_config_path();
        let local = local.to_str().unwrap();
        let missing = scratch("no-such-config");
        let missing = missing.to_str().unwrap();
        let cases: [(&str, Vec<&str>, &str); 4] = [
            ("a missing argument", vec!["doctor"], "usage"),
            (
                "--config without --node",
                vec!["doctor", "dir", "--config", local],
                "usage",
            ),
            (
                "an unreadable config",
                vec!["doctor", "dir", "--config", missing, "--node", "1"],
                "config",
            ),
            (
                "--node 0",
                vec!["doctor", "dir", "--config", local, "--node", "0"],
                "usage",
            ),
        ];
        for (case, args, kind) in cases {
            let mut with_json = args.clone();
            with_json.push("--json");
            let output = keyval(&with_json);
            let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
                panic!(
                    "rule: under --json even an error is one JSON document on stdout (O-6); \
                     case={case}; {err}; stdout={}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
            let errors: Vec<String> = validator
                .iter_errors(&report)
                .map(|e| e.to_string())
                .collect();
            assert_eq!(
                (
                    errors,
                    report["error"]["kind"].as_str(),
                    output.status.code()
                ),
                (Vec::<String>::new(), Some(kind), Some(2)),
                "rule: a {kind} error is a keyval.cli.error/v1 envelope, exit 2 (O-6, §11.13); \
                 case={case} report={report}"
            );
            let plain = keyval(&args);
            assert_eq!(
                (
                    plain.stdout.is_empty(),
                    plain.stderr.is_empty(),
                    plain.status.code()
                ),
                (true, false, Some(2)),
                "rule: without --json the error stays text on stderr, exit 2; case={case}"
            );
        }
        let listed: BTreeSet<String> = schema
            .pointer("/properties/error/properties/kind/enum")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect();
        assert_eq!(
            listed,
            every!(ErrorKind: Usage, Config),
            "rule: the error schema's kinds are exactly what the CLI serialises (O-6)"
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
