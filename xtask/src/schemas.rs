//! JSON Schemas for the versioned `./dev --json` outputs (O-6, §11.5 rule 14; check
//! DEV-SCHEMAS).
//!
//! Every identifier the code emits (`keyval.dev.<name>/v<n>`) has exactly one schema,
//! `schemas/dev/<name>.v<n>.schema.json`, and one negative fixture,
//! `schemas/dev/invalid/<name>.v<n>.json`, which the schema must reject at the JSON
//! pointer the fixture names. The tests validate real outputs: each command's own code
//! produces them, with stand-ins only for the tracker, Agent Mail and the gate's long
//! builds. So an output that changes without its schema fails here, and so does a new
//! enum variant the schema does not list. `xtask/tests/schemas.rs` validates the `./dev`
//! binary's own stdout for the commands that are cheap to run.
//!
//! Test-only: never compiled into `./dev`.
#![cfg(test)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Draft, Registry, Validator};
use serde::Serialize;
use serde_json::{Value, json};

use crate::test_support::{manifest_dir, workspace_root};
use crate::tier_plan::{Reason as SelectionReason, Suite};

/// Where the schemas live, relative to the workspace root.
const DIR: &str = "schemas/dev";

/// Where the negative fixtures live, relative to the workspace root.
const INVALID_DIR: &str = "schemas/dev/invalid";

/// The base of every schema's `$id`. The `.invalid` host is never resolved: references
/// between the schemas are served from the files themselves, offline.
const BASE_URI: &str = "https://keyval.invalid/schemas/dev/";

/// The dialect every schema declares.
const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Every versioned identifier `./dev` emits. A new identifier or a version bump is
/// listed here, with its schema and its negative fixture.
const IDS: &[&str] = &[
    crate::output::ERROR_SCHEMA,
    crate::corpus::SCHEMA,
    crate::doctor::SCHEMA,
    crate::check::SCHEMA,
    crate::gate::SCHEMA,
    crate::gate::RECEIPT_SCHEMA,
    crate::accept::SCHEMA,
    crate::commit::SCHEMA,
    crate::closure::SCHEMA,
    crate::beads::SCHEMA,
    crate::prune::SCHEMA,
    crate::schedule::SCHEMA,
    crate::steplog::SCHEMA,
    crate::traceability::SCHEMA,
    crate::coverage::SCHEMA,
    crate::sim::SCHEMA,
    crate::sim_campaign::RUN_SCHEMA,
    crate::sim_campaign::CAMPAIGN_SCHEMA,
    crate::sim_campaign::CRASHPACK_SCHEMA,
    crate::sim_campaign::BUILD_SCHEMA,
    crate::demo::SCHEMA,
    crate::cluster::SCHEMA,
    crate::lifecycle::SCHEMA,
];

/// The file stem for an identifier: `keyval.dev.<name>/v<n>` becomes `<name>.v<n>`.
fn stem(id: &str) -> Option<String> {
    let (name, version) = id.strip_prefix("keyval.dev.")?.split_once("/v")?;
    let well_formed = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit());
    well_formed.then(|| format!("{name}.v{version}"))
}

fn schema_path(id: &str) -> PathBuf {
    let stem = stem(id).unwrap_or_else(|| panic!("malformed identifier {id:?}"));
    workspace_root()
        .join(DIR)
        .join(format!("{stem}.schema.json"))
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not JSON: {err}", path.display()))
}

/// The schema document of each identifier.
fn documents() -> BTreeMap<&'static str, Value> {
    IDS.iter()
        .map(|&id| (id, read_json(&schema_path(id))))
        .collect()
}

/// A compiled validator for each identifier, with format assertions on. Every schema is
/// registered under its `$id`, so a `$ref` to another schema resolves offline.
struct Validators(BTreeMap<&'static str, Validator>);

impl Validators {
    fn load() -> Self {
        let documents = documents();
        let resources = documents.values().map(|doc| {
            let uri = doc["$id"].as_str().unwrap_or_default().to_owned();
            (uri, doc.clone())
        });
        let registry = Registry::new()
            .draft(Draft::Draft202012)
            .extend(resources)
            .and_then(jsonschema::RegistryBuilder::prepare)
            .unwrap_or_else(|err| panic!("{DIR} cannot be registered: {err}"));
        let compiled = documents
            .iter()
            .map(|(&id, doc)| {
                let validator = jsonschema::options()
                    .with_draft(Draft::Draft202012)
                    .with_registry(&registry)
                    .should_validate_formats(true)
                    .build(&json!({ "$ref": doc["$id"] }))
                    .unwrap_or_else(|err| panic!("the schema of {id} does not compile: {err}"));
                (id, validator)
            })
            .collect();
        Self(compiled)
    }

    /// Where `instance` breaks the schema that its own `schema` field names, as
    /// `(JSON pointer, message)`. A missing or unknown identifier is one violation.
    fn violations(&self, instance: &Value) -> Vec<(String, String)> {
        let id = instance
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(validator) = self.0.get(id) else {
            return vec![("/schema".to_owned(), format!("no schema for {id:?}"))];
        };
        validator
            .iter_errors(instance)
            .map(|err| (err.instance_path().as_str().to_owned(), err.to_string()))
            .collect()
    }

    /// Asserts that `output`, serialised, matches its schema.
    #[track_caller]
    fn assert_matches(&self, output: &impl Serialize, source: &str) -> Value {
        let instance = serde_json::to_value(output)
            .unwrap_or_else(|err| panic!("cannot serialise {source}: {err}"));
        let found = self.violations(&instance);
        assert!(
            found.is_empty(),
            "rule: every ./dev JSON output matches the schema its `schema` field names, so \
             an output never changes without its schema (O-6); source={source}; \
             violations={found:#?}; instance={instance:#}"
        );
        instance
    }
}

/// Every variant of an enum, serialised. The match is exhaustive, so a new variant fails
/// to compile until it is listed here, and then fails `enums_match_their_schemas` until
/// the schema lists it too.
macro_rules! variants {
    ($ty:ident: $($variant:ident),+ $(,)?) => {{
        let all = [$($ty::$variant),+];
        for value in &all {
            match value {
                $($ty::$variant)|+ => {}
            }
        }
        all.iter()
            .map(|value| serde_json::to_value(value).unwrap())
            .collect::<Vec<Value>>()
    }};
}

#[cfg(test)]
mod tests {
    // check: DEV-SCHEMAS
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use super::*;
    use crate::accept::{self, SelectorResult, SelectorRunner, SelectorSource};
    use crate::cargo::Runner;
    use crate::check::{self, CATALOGUE, CheckStatus, ClaimClass, Kind, StaticCheck, Tier};
    use crate::closure::CloseReport;
    use crate::cluster::{self, Action as ClusterAction, NodeState};
    use crate::commit::{self, CommitKind, CommitReport, GateRun, Outcome as CommitOutcome};
    use crate::commit::{GateCall, Ports, Request, Reservation};
    use crate::demo;
    use crate::doctor::{self, Expect, Need, Probe, ToolSpec, ToolStatus};
    use crate::gate::{self, Context, GateTier, Step as GateStep, Tree, TreeSource, Verdict};
    use crate::git::{Git, fixture};
    use crate::lifecycle;
    use crate::output::{self, ErrorKind};
    use crate::prune::{self, Action, Layout, Reason};
    use crate::sim::{self, Verdict as SimVerdict};
    use crate::sim_campaign::{self, CampaignRequest};
    use crate::staging::Field as DemotionField;
    use crate::steplog::{self, Outcome as StepOutcome, StepLog};
    use crate::test_support::{nested_cargo, scratch_dir};
    use crate::traceability::{self, InputSource, Source as EvidenceSource};
    use crate::tracker::{Bead, Tracker};
    use crate::{beads, vendor};

    // --- The catalogue: one schema and one negative fixture per identifier. ---

    fn listed(dir: &Path, suffix: &str) -> BTreeSet<String> {
        fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("cannot list {}: {err}", dir.display()))
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter_map(|name| name.strip_suffix(suffix).map(str::to_owned))
            .collect()
    }

    #[test]
    fn every_identifier_has_one_schema_and_one_negative_fixture() {
        let stems: BTreeSet<String> = IDS
            .iter()
            .map(|id| stem(id).unwrap_or_else(|| panic!("malformed identifier {id:?}")))
            .collect();
        assert_eq!(
            stems.len(),
            IDS.len(),
            "rule: identifiers are unique; IDS={IDS:?}"
        );
        let root = workspace_root();
        let schemas = listed(&root.join(DIR), ".schema.json");
        assert_eq!(
            schemas, stems,
            "rule: {DIR} holds exactly one schema per emitted identifier; a bumped version \
             replaces its predecessor's file (O-6)"
        );
        let invalid = listed(&root.join(INVALID_DIR), ".json");
        assert_eq!(
            invalid, stems,
            "rule: {INVALID_DIR} holds exactly one negative fixture per schema"
        );
    }

    /// The `"keyval.dev.…"` string literals in `source`.
    fn identifier_literals(source: &str) -> Vec<String> {
        source
            .split("\"keyval.dev.")
            .skip(1)
            .filter_map(|rest| rest.split_once('"'))
            .map(|(id, _)| format!("keyval.dev.{id}"))
            .collect()
    }

    #[test]
    fn every_identifier_the_code_emits_is_catalogued() {
        let src = manifest_dir().join("src");
        let mut found = BTreeMap::<String, Vec<String>>::new();
        for entry in fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            // This module names the identifiers' common prefix, which is no identifier.
            if path.extension().is_none_or(|e| e != "rs") || path.ends_with("schemas.rs") {
                continue;
            }
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            for id in identifier_literals(&fs::read_to_string(&path).unwrap()) {
                found.entry(id).or_default().push(file.clone());
            }
        }
        let catalogued: BTreeSet<&str> = IDS.iter().copied().collect();
        let uncatalogued: Vec<_> = found
            .iter()
            .filter(|(id, _)| !catalogued.contains(id.as_str()))
            .collect();
        assert!(
            uncatalogued.is_empty(),
            "rule: every identifier in xtask/src has a schema in {DIR}, listed in \
             schemas::IDS; uncatalogued={uncatalogued:?}"
        );
        let unused: Vec<&&str> = catalogued
            .iter()
            .filter(|id| !found.contains_key(**id))
            .collect();
        assert!(
            unused.is_empty(),
            "rule: IDS lists only identifiers the code emits; unused={unused:?}"
        );
    }

    #[test]
    fn each_schema_is_closed_and_names_its_identifier() {
        for (id, doc) in documents() {
            let file = format!("{}.schema.json", stem(id).unwrap());
            let at = |pointer: &str| doc.pointer(pointer).cloned().unwrap_or(Value::Null);
            let expected = [
                ("/$schema", json!(DIALECT)),
                ("/$id", json!(format!("{BASE_URI}{file}"))),
                ("/title", json!(id)),
                ("/properties/schema/const", json!(id)),
                ("/additionalProperties", json!(false)),
            ];
            for (pointer, value) in expected {
                assert_eq!(
                    at(pointer),
                    value,
                    "rule: each schema is a closed 2020-12 document whose `schema` const is \
                     its identifier; file={file} pointer={pointer}"
                );
            }
            let properties: BTreeSet<&str> = doc["properties"]
                .as_object()
                .map(|props| props.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let required: BTreeSet<&str> = doc["required"]
                .as_array()
                .map(|req| req.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            assert_eq!(
                required, properties,
                "rule: every field is required, because the reports serialise every field, \
                 null included; file={file}"
            );
        }
    }

    #[test]
    fn each_negative_fixture_is_rejected_where_it_says() {
        let validators = Validators::load();
        for id in IDS {
            let path = workspace_root()
                .join(INVALID_DIR)
                .join(format!("{}.json", stem(id).unwrap()));
            let fixture = read_json(&path);
            let rule = fixture["rule"].as_str().unwrap_or_default();
            let error_at = fixture["error_at"].as_str();
            let instance = &fixture["instance"];
            assert!(
                !rule.is_empty() && error_at.is_some(),
                "rule: a negative fixture states the rule it breaks and where; path={}",
                path.display()
            );
            assert_eq!(
                instance["schema"],
                json!(id),
                "rule: a negative fixture carries its own identifier, so it is rejected for \
                 its defect and not for a wrong `schema`; path={}",
                path.display()
            );
            let found = validators.violations(instance);
            assert!(
                found.iter().any(|(at, _)| Some(at.as_str()) == error_at),
                "rule: {rule}; the schema must reject the fixture at {error_at:?}; path={} \
                 violations={found:#?}",
                path.display()
            );
        }
    }

    // --- Every serialised enum variant is in its schema's `enum`, and nothing else. ---

    fn assert_enum(id: &str, pointer: &str, variants: &[Value], nullable: bool) {
        let doc = read_json(&schema_path(id));
        let listed: BTreeSet<String> = doc
            .pointer(pointer)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{id} has no enum at {pointer}"))
            .iter()
            .map(Value::to_string)
            .collect();
        let mut expected: BTreeSet<String> = variants.iter().map(Value::to_string).collect();
        if nullable {
            expected.insert(Value::Null.to_string());
        }
        assert_eq!(
            listed, expected,
            "rule: the schema's enum lists exactly the variants the code serialises; \
             schema={id} pointer={pointer}"
        );
    }

    fn cluster_enums_match() {
        let actions = variants!(ClusterAction: Init, Up, Status, Down, Kill, Restart);
        assert_enum(cluster::SCHEMA, "/properties/action/enum", &actions, false);
        let states = variants!(
            NodeState: Uninitialised,
            Initialised,
            Stamped,
            Running,
            Started,
            Stopped,
            Killed,
            Stale,
            Refused
        );
        assert_enum(
            cluster::SCHEMA,
            "/$defs/node/properties/state/enum",
            &states,
            false,
        );
    }

    #[test]
    fn enums_match_their_schemas() {
        cluster_enums_match();
        let verdicts = variants!(SimVerdict: Pass, Fail, Incomplete);
        assert_enum(sim::SCHEMA, "/properties/verdict/enum", &verdicts, false);
        let error = output::ERROR_SCHEMA;
        let doctor = doctor::SCHEMA;
        let check = check::SCHEMA;
        let receipt = gate::RECEIPT_SCHEMA;
        let kinds = variants!(ErrorKind: Usage, Internal, Refused, NotImplemented);
        assert_enum(
            error,
            "/properties/error/properties/kind/enum",
            &kinds,
            false,
        );
        let needs = variants!(Need: Required, Optional);
        assert_enum(doctor, "/$defs/tool/properties/need/enum", &needs, false);
        let statuses = variants!(ToolStatus: Ok, Missing, Broken, Mismatch);
        assert_enum(
            doctor,
            "/$defs/tool/properties/status/enum",
            &statuses,
            false,
        );
        let statuses = variants!(CheckStatus: Passed, Failed, Unknown, NotRun);
        assert_enum(
            check,
            "/$defs/result/properties/status/enum",
            &statuses,
            false,
        );
        let kinds = variants!(Kind: TestGroup, Static, Checker);
        assert_enum(check, "/$defs/result/properties/kind/enum", &kinds, true);
        let tiers = variants!(Tier: T0);
        assert_enum(check, "/$defs/result/properties/tier/enum", &tiers, true);
        let classes = variants!(ClaimClass: Invariant, SimulationEvidenced);
        assert_enum(check, "/$defs/result/properties/class/enum", &classes, true);
        let runners = variants!(Runner: Rch, Local);
        assert_enum(check, "/properties/runner/enum", &runners, false);
        let pointer = "/properties/tools/properties/cargo_runner/enum";
        assert_enum(receipt, pointer, &runners, false);
        let tiers = variants!(GateTier: Fast, Integration, Campaign, Release);
        assert_enum(receipt, "/properties/tier/enum", &tiers, false);
        let sources = variants!(TreeSource: GitHead, Caller);
        assert_enum(
            receipt,
            "/properties/tree/properties/source/enum",
            &sources,
            false,
        );
        let verdicts = variants!(Verdict: Passed, Failed);
        assert_enum(receipt, "/$defs/verdict/enum", &verdicts, false);
        let sources = variants!(SelectorSource: Workgraph, Labels);
        assert_enum(accept::SCHEMA, "/properties/source/enum", &sources, true);
        let outcomes = variants!(CommitOutcome: Committed, Refused, GateFailed);
        assert_enum(commit::SCHEMA, "/properties/outcome/enum", &outcomes, false);
        let actions = variants!(Action: Kept, Removed, WouldRemove);
        assert_enum(prune::SCHEMA, "/$defs/action/enum", &actions, false);
        let reasons = variants!(
            Reason: Newest,
            Failing,
            NoReceipt,
            InUse,
            Superseded,
            CheckoutExists,
            CheckoutGone,
            Unattributed,
            MarkerMismatch,
        );
        assert_enum(prune::SCHEMA, "/$defs/reason/enum", &reasons, false);
        let outcomes = variants!(StepOutcome: Passed, Failed);
        assert_enum(
            steplog::SCHEMA,
            "/properties/outcome/enum",
            &outcomes,
            false,
        );
        let sources = variants!(EvidenceSource: Accept, Gate);
        assert_enum(
            traceability::SCHEMA,
            "/$defs/evidence/properties/source/enum",
            &sources,
            false,
        );
        let fields = variants!(DemotionField: Status, MinCases, Tier);
        assert_enum(
            gate::RECEIPT_SCHEMA,
            "/properties/demotions/items/properties/field/enum",
            &fields,
            false,
        );
        let inputs = variants!(InputSource: Tree, WorkingTree);
        assert_enum(
            traceability::SCHEMA,
            "/properties/inputs/enum",
            &inputs,
            false,
        );
    }

    #[test]
    fn tier_plan_enums_match_their_schema() {
        let receipt = gate::RECEIPT_SCHEMA;
        let reasons = variants!(
            SelectionReason: TierSelectsWholeWorkspace,
            NoChangedPaths,
            SharedPathChanged,
            ChangedMembers,
            NothingBuilt
        );
        let pointer = "/properties/tier_plan/properties/selection/properties/reason/enum";
        assert_enum(receipt, pointer, &reasons, false);
        let suites = variants!(Suite: Conformance, RealFileProbes, BugVariants);
        let pointer = "/properties/tier_plan/properties/reused/items/properties/suite/enum";
        assert_enum(receipt, pointer, &suites, false);
    }

    // --- Real outputs, produced by each command's own code. ---

    #[test]
    fn error_envelopes_match_their_schema() {
        let validators = Validators::load();
        for kind in [
            ErrorKind::Usage,
            ErrorKind::Internal,
            ErrorKind::Refused,
            ErrorKind::NotImplemented,
        ] {
            let json = output::error_json(kind, "the arguments named no command").unwrap();
            let envelope: Value = serde_json::from_str(&json).unwrap();
            validators.assert_matches(&envelope, &format!("output::error_json({kind:?})"));
        }
    }

    /// A tool for each status doctor can report, judged against a pin no rustc has.
    const PROBED: &[ToolSpec] = &[
        ToolSpec {
            name: "echo",
            purpose: "a tool that is present and answers",
            need: Need::Required,
            version: Probe {
                argv: &["echo", "schemas-probe 1.0"],
                expect: Expect::FirstLine,
            },
            capability: Some(Probe {
                argv: &["echo", "capable"],
                expect: Expect::FirstLine,
            }),
        },
        ToolSpec {
            name: "keyval-schemas-no-such-tool",
            purpose: "a tool that is not installed",
            need: Need::Optional,
            version: Probe {
                argv: &["keyval-schemas-no-such-tool", "--version"],
                expect: Expect::FirstLine,
            },
            capability: None,
        },
        ToolSpec {
            name: "false",
            purpose: "a tool whose probe fails",
            need: Need::Required,
            version: Probe {
                argv: &["false"],
                expect: Expect::FirstLine,
            },
            capability: None,
        },
        ToolSpec {
            name: "rustc",
            purpose: "a tool that is not the pinned version",
            need: Need::Required,
            version: Probe {
                argv: &["rustc", "-vV"],
                expect: Expect::PinnedRustc,
            },
            capability: None,
        },
    ];

    #[test]
    fn doctor_reports_match_their_schema() {
        let validators = Validators::load();
        let root = workspace_root();
        let probed = doctor::run(
            &root,
            Ok("0.0.0".to_owned()),
            PROBED,
            vendor::codegen_pin(&root),
        );
        let seen: BTreeSet<String> = probed
            .tools
            .iter()
            .map(|tool| format!("{:?}", tool.status))
            .collect();
        assert_eq!(
            seen.len(),
            4,
            "the probes reach every tool status; report={probed:#?}"
        );
        validators.assert_matches(&probed, "doctor::run with one tool per status");
        let unpinned = doctor::run(
            &root,
            Err("rust-toolchain.toml is missing".to_owned()),
            &[],
            vendor::codegen_pin(&scratch_dir("schemas-doctor-no-vendor")),
        );
        validators.assert_matches(&unpinned, "doctor::run with no pin and no vendored Raft");
    }

    #[test]
    fn check_reports_match_their_schema() {
        let validators = Validators::load();
        let root = workspace_root();
        let cargo = nested_cargo("keyval-schemas");
        let run = |root: &Path, ids: &[&str]| {
            let ids: Vec<String> = ids.iter().map(|&id| id.to_owned()).collect();
            check::run(root, CATALOGUE, &ids, None, &cargo)
        };
        let empty = scratch_dir("schemas-check-empty");
        let reports = [
            ("a passing static check", run(&root, &["AGENTS-RULES"])),
            ("an unknown ID", run(&root, &["NOPE", "AGENTS-RULES"])),
            ("no IDs", run(&root, &[])),
            ("a failing static check", run(&empty, &["AGENTS-RULES"])),
        ];
        let mut seen = BTreeSet::new();
        for (case, report) in &reports {
            seen.extend(report.checks.iter().map(|c| format!("{:?}", c.status)));
            validators.assert_matches(report, &format!("check::run: {case}"));
        }
        assert_eq!(
            seen.len(),
            4,
            "the runs reach every check status; seen={seen:?}"
        );
    }

    #[test]
    fn gate_reports_receipts_and_step_records_match_their_schemas() {
        let validators = Validators::load();
        let root = manifest_dir().join("tests/fixtures/gate-ok");
        let mut verdicts = BTreeSet::new();
        for check in [StaticCheck::LintAllows, StaticCheck::DepLedger] {
            let ctx = Context {
                root: &root,
                tree: Tree {
                    id: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    source: TreeSource::Caller,
                    changed: vec!["src/lib.rs".to_owned()],
                },
                artifacts: scratch_dir("schemas-gate"),
                cargo: nested_cargo("keyval-schemas"),
                tool_path: None,
                progress: false,
                parent: gate::Parent::Absent,
                allow_demotions: Vec::new(),
            };
            let report = gate::run(&ctx, GateTier::Fast, &[GateStep::Static(check)]).unwrap();
            verdicts.insert(format!("{:?}", report.receipt.result));
            let source = format!("gate::run with {}", check.id());
            validators.assert_matches(&report, &source);
            let written = read_json(Path::new(&report.receipt_path));
            validators.assert_matches(&written, &format!("{source}: the receipt file"));
            let steps = report.receipt.step_log.path.as_deref().unwrap();
            let records = fs::read_to_string(steps).unwrap();
            assert!(
                !records.is_empty(),
                "the gate logged its step; path={steps}"
            );
            for line in records.lines() {
                let record: Value = serde_json::from_str(line).unwrap();
                validators.assert_matches(&record, &format!("{source}: {steps}"));
            }
        }
        assert_eq!(
            verdicts.len(),
            2,
            "the runs reach both verdicts; seen={verdicts:?}"
        );
        // The registry set's receipt (W05.4), with a planned row and an approved demotion.
        let parent = gate::tests::registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 2),
            ("R-OLD", "implemented", 1),
        ]);
        let tree = gate::tests::registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 2),
            ("R-LATER", "planned", 1),
        ]);
        let root = gate::tests::registry_fixture("schemas", &tree);
        let report =
            gate::tests::registry_run(&root, gate::Parent::Checks(parent), &["R-OLD=br-pc"]);
        assert!(
            report.ok && !report.receipt.planned.is_empty() && !report.receipt.demotions.is_empty(),
            "the run reports a planned row and a demotion; report={report:#?}"
        );
        validators.assert_matches(&report, "gate::run_registry");
        let written = read_json(Path::new(&report.receipt_path));
        validators.assert_matches(&written, "gate::run_registry: the receipt file");
    }

    /// Stand-in commit ports: the bead has `status`, one reservation covers everything,
    /// and the gate returns `passed`.
    struct CommitPorts {
        status: &'static str,
        passed: bool,
    }

    impl Ports for CommitPorts {
        fn bead_status(&self, _bead: &str) -> Result<String, String> {
            Ok(self.status.to_owned())
        }
        fn reservations(&self, agent: &str) -> Result<Vec<Reservation>, String> {
            Ok(vec![Reservation {
                agent: agent.to_owned(),
                pattern: "**".to_owned(),
                exclusive: true,
                remaining_seconds: 600,
            }])
        }
        fn gate(&self, call: &GateCall<'_>) -> Result<GateRun, String> {
            Ok(GateRun {
                passed: self.passed,
                receipt: Some(format!("artifacts/receipts/gate-fast-{}.json", call.tree)),
                summary: if self.passed { "passed" } else { "fmt failed" }.to_owned(),
            })
        }
        fn push(&self, _git: &Git) -> Vec<String> {
            vec!["main: pushed (stand-in)".to_owned()]
        }
    }

    /// A real commit-wrapper run on a scratch repository.
    fn commit_report(name: &str, ports: &CommitPorts) -> CommitReport {
        let git = fixture::repo(name, &[("a.rs", "a\n")]);
        fixture::write(git.root(), "a.rs", "a2\n");
        let request = Request {
            kind: CommitKind::Work,
            bead: "br-schemas".to_owned(),
            agent: "SchemaAgent".to_owned(),
            message: "schemas: a test commit".to_owned(),
            paths: vec!["a.rs".to_owned()],
            allow_demotions: Vec::new(),
        };
        commit::run(
            &git,
            &request,
            ports,
            &scratch_dir(&format!("{name}-artifacts")),
        )
    }

    #[test]
    fn commit_and_close_reports_match_their_schemas() {
        let validators = Validators::load();
        let committed = commit_report(
            "schemas-commit",
            &CommitPorts {
                status: "in_progress",
                passed: true,
            },
        );
        let others = [
            commit_report(
                "schemas-commit-refused",
                &CommitPorts {
                    status: "open",
                    passed: true,
                },
            ),
            commit_report(
                "schemas-commit-gate-failed",
                &CommitPorts {
                    status: "in_progress",
                    passed: false,
                },
            ),
        ];
        let mut seen = BTreeSet::new();
        for report in others.iter().chain([&committed]) {
            seen.insert(format!("{:?}", report.outcome));
            validators.assert_matches(report, &format!("commit::run: {:?}", report.outcome));
            let path = report.receipt_path.as_deref().unwrap();
            validators.assert_matches(&read_json(Path::new(path)), "the commit receipt file");
        }
        assert_eq!(seen.len(), 3, "the runs reach every outcome; seen={seen:?}");

        // A closure's report embeds the commit wrapper's report.
        let closed = CloseReport {
            schema: crate::closure::SCHEMA,
            ok: true,
            mode: "close",
            bead: committed.bead.clone(),
            accept_receipt: Some("artifacts/receipts/accept-br-schemas.json".to_owned()),
            work_commit: committed.commit.clone(),
            work_tree: committed.tree.clone(),
            activated: vec!["O-6".to_owned()],
            demoted: vec!["F-1".to_owned()],
            commit: Some(committed),
            detail: None,
        };
        validators.assert_matches(&closed, "a closure that committed");
        let failed = CloseReport {
            schema: crate::closure::SCHEMA,
            ok: false,
            mode: "failed",
            bead: "br-schemas".to_owned(),
            accept_receipt: None,
            work_commit: None,
            work_tree: None,
            activated: Vec::new(),
            demoted: Vec::new(),
            commit: None,
            detail: Some("the evidence file is missing".to_owned()),
        };
        validators.assert_matches(&failed, "a failure record that stopped early");
    }

    /// A stand-in tracker whose beads carry `labels`.
    struct Labelled(Vec<String>);

    impl Tracker for Labelled {
        fn bead(&self, id: &str) -> Result<Bead, String> {
            Ok(Bead {
                id: id.to_owned(),
                status: "in_progress".to_owned(),
                labels: self.0.clone(),
            })
        }
        fn all_beads(&self) -> Result<Vec<Bead>, String> {
            Ok(Vec::new())
        }
        fn close(&self, _id: &str, _reason: &str) -> Result<(), String> {
            Err("read-only stand-in".to_owned())
        }
        fn mark_failed(&self, _id: &str, _comment: &str) -> Result<(), String> {
            Err("read-only stand-in".to_owned())
        }
        fn flush(&self) -> Result<(), String> {
            Err("read-only stand-in".to_owned())
        }
        fn reopen(&self, _id: &str, _status: &str) -> Result<(), String> {
            Err("read-only stand-in".to_owned())
        }
        fn unmark_failed(&self, _id: &str) -> Result<(), String> {
            Err("read-only stand-in".to_owned())
        }
    }

    /// A selector runner that records its arguments and reports `ok`.
    struct Selectors {
        ok: bool,
        runs: RefCell<Vec<Vec<String>>>,
    }

    impl SelectorRunner for Selectors {
        fn run(&self, _dir: &Path, _target: &Path, args: &[String]) -> SelectorResult {
            self.runs.borrow_mut().push(args.to_vec());
            SelectorResult {
                selector: String::new(),
                ok: self.ok,
                checks: vec![format!(
                    "{} {}",
                    args.join(" "),
                    if self.ok { "passed 3" } else { "failed 1" }
                )],
                detail: (!self.ok).then(|| "1 case failed".to_owned()),
            }
        }
    }

    #[test]
    fn accept_reports_match_their_schema() {
        let validators = Validators::load();
        let git = fixture::repo("schemas-accept", &[("a.rs", "a\n")]);
        let head_tree = git.rev_parse("HEAD^{tree}").unwrap();
        let tracker = Labelled(vec!["accept:XTASK".to_owned(), "m0".to_owned()]);
        let mut sources = BTreeSet::new();
        for (judged, ok) in [
            (head_tree.as_str(), true),
            (head_tree.as_str(), false),
            ("not-a-tree", true),
        ] {
            let runner = Selectors {
                ok,
                runs: RefCell::new(Vec::new()),
            };
            let artifacts = scratch_dir("schemas-accept-artifacts");
            let report = accept::run(&git, &tracker, &runner, "br-schemas", judged, &artifacts);
            sources.insert(format!("{:?}", report.source));
            let case = format!("accept::run tree={judged} selectors_ok={ok}");
            validators.assert_matches(&report, &case);
            if let Some(path) = report.receipt_path.as_deref() {
                validators.assert_matches(&read_json(Path::new(path)), &format!("{case}: receipt"));
            }
        }
        assert!(
            sources.contains("Some(Labels)") && sources.contains("None"),
            "the runs reach a labelled selection and an early refusal; seen={sources:?}"
        );
    }

    /// A tracker graph for the importer: `beads` answers with `found`, and every change
    /// is refused (only dry runs use it).
    struct Graph(Result<Vec<beads::TrackedBead>, String>);

    impl beads::BeadGraph for Graph {
        fn beads(&self) -> Result<Vec<beads::TrackedBead>, String> {
            self.0.clone()
        }
        fn blocks(&self, _ids: &[String]) -> Result<BTreeMap<String, Vec<String>>, String> {
            Ok(BTreeMap::new())
        }
        fn create(&self, _bead: &beads::NewBead) -> Result<String, String> {
            Err("dry-run stand-in".to_owned())
        }
        fn add_blocks(&self, _bead: &str, _on: &str) -> Result<(), String> {
            Err("dry-run stand-in".to_owned())
        }
        fn remove_blocks(&self, _bead: &str, _on: &str) -> Result<(), String> {
            Err("dry-run stand-in".to_owned())
        }
        fn add_parent(&self, _bead: &str, _epic: &str) -> Result<(), String> {
            Err("dry-run stand-in".to_owned())
        }
        fn flush(&self) -> Result<(), String> {
            Err("dry-run stand-in".to_owned())
        }
    }

    #[test]
    fn beads_import_reports_match_their_schema() {
        let validators = Validators::load();
        let root = workspace_root();
        let epics = ["m0", "m1", "m2", "m3", "m4", "m5"].map(|m| beads::TrackedBead {
            id: format!("br-epic-{m}"),
            labels: vec!["milestone".to_owned(), m.to_owned()],
        });
        let graphs = [
            (
                "an empty tracker with its milestone epics",
                Graph(Ok(epics.to_vec())),
            ),
            ("an empty tracker", Graph(Ok(Vec::new()))),
            (
                "an unreachable tracker",
                Graph(Err("br is not on PATH".to_owned())),
            ),
        ];
        for (case, graph) in &graphs {
            let report = beads::run(&root, graph, true);
            validators.assert_matches(&report, &format!("beads::run --dry-run: {case}"));
        }
    }

    #[test]
    fn sim_run_reports_match_their_schema() {
        let validators = Validators::load();
        let passing = sim::parse(sim::tests::PASSING).unwrap();
        validators.assert_matches(&passing, "sim::parse (seed 42's report)");
        let mut failed = passing.clone();
        failed.ok = false;
        failed.verdict = SimVerdict::Fail;
        failed.facts.mirrors_audited = false;
        failed.failures = vec!["the mirror audit failed".to_owned()];
        validators.assert_matches(&failed, "a failed run");
        let mut incomplete = passing;
        incomplete.ok = false;
        incomplete.verdict = SimVerdict::Incomplete;
        incomplete.error = Some("incomplete after 150 events".to_owned());
        incomplete.trace.complete = false;
        incomplete.facts.restarted = None;
        validators.assert_matches(&incomplete, "an incomplete run");
    }

    #[test]
    fn scenario_run_verdicts_match_their_schema() {
        let verdicts = variants!(SimVerdict: Pass, Fail, Incomplete);
        assert_enum(
            sim_campaign::RUN_SCHEMA,
            "/properties/verdict/enum",
            &verdicts,
            false,
        );
    }

    #[test]
    fn sim_scenario_run_reports_match_their_schema() {
        let validators = Validators::load();
        let text = sim_campaign::tests::report_text(7, "incomplete");
        let incomplete = sim_campaign::parse(&text).unwrap();
        validators.assert_matches(&incomplete, "an unjudged scenario run (incomplete)");
        let mut failed = incomplete.clone();
        failed.ok = false;
        failed.verdict = SimVerdict::Fail;
        failed.incomplete = Vec::new();
        failed.failures = vec!["the heal used its whole budget".to_owned()];
        validators.assert_matches(&failed, "a failed scenario run");
        failed.crash_pack = Some("artifacts/crashpacks/7-0123abcd".to_owned());
        validators.assert_matches(&failed, "a failed scenario run that wrote a crash pack");
        let mut judged = incomplete;
        judged.ok = true;
        judged.verdict = SimVerdict::Pass;
        judged.incomplete = Vec::new();
        judged.checkers.ran = vec!["K-1".to_owned()];
        judged.checkers.not_run = Vec::new();
        validators.assert_matches(&judged, "a judged, passing scenario run");
        let mut unjudged_pass = judged;
        unjudged_pass.checkers.ran = Vec::new();
        let instance = serde_json::to_value(&unjudged_pass).unwrap();
        assert!(
            !validators.violations(&instance).is_empty(),
            "rule: a pass that no checker judged is rejected by the schema (P10); {instance}"
        );
    }

    #[test]
    fn corpus_run_reports_match_their_schema() {
        let validators = Validators::load();
        let root = crate::test_support::workspace_root();
        let fake = crate::corpus::tests::AlwaysPass;
        let report = crate::corpus::run(&root, crate::corpus::Selection::Fast, &fake);
        validators.assert_matches(&report, "a passing fast-subset run of the committed corpus");
        let mut failed = report;
        failed.ok = false;
        failed.counts.pass -= 1;
        failed.counts.fail = 1;
        failed.cases[0].verdict = crate::corpus::CaseVerdict::Fail;
        failed.cases[0].reason = Some("one failed".to_owned());
        validators.assert_matches(&failed, "a corpus run with a failing case");
        let unverified = crate::corpus::run(
            &crate::test_support::scratch_dir("schemas-corpus-none"),
            crate::corpus::Selection::All,
            &fake,
        );
        validators.assert_matches(&unverified, "a corpus that does not verify");
    }

    #[test]
    fn crash_pack_build_identities_match_their_schema() {
        let inputs = crate::gate::BuildInputs {
            rustc: "rustc 1.99.0".to_owned(),
            cargo: "cargo 1.99.0".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            lockfile_sha256: "c".repeat(64),
            features: "all",
            raft_patch_sha256: None,
            raft_vendor_sha256: Some("d".repeat(64)),
        };
        let identity =
            sim_campaign::build_identity(&"a".repeat(40), &"b".repeat(40), true, &inputs);
        Validators::load().assert_matches(&identity, "a crash pack's build identity");
    }

    #[test]
    fn sim_campaign_reports_match_their_schema() {
        use std::os::unix::process::ExitStatusExt as _;
        let validators = Validators::load();
        let request = CampaignRequest {
            from: 1,
            to: 3,
            profile: sim_campaign::SWARM.to_owned(),
            config: PathBuf::from("/r/c.toml"),
            jobs: 2,
        };
        for (case, failing) in [("all pass", None), ("one incomplete, one error", Some(2))] {
            let report = sim_campaign::campaign_with(
                &request,
                &scratch_dir("schemas-sim-campaign"),
                &|args: &[&str]| {
                    let at = args.iter().position(|arg| *arg == "--seed").unwrap();
                    let seed: u64 = args[at + 1].parse().unwrap();
                    let (verdict, code) = match (failing, seed) {
                        (Some(bad), seed) if seed == bad => ("incomplete", 1),
                        (Some(_), 3) => return Err("cargo: not found".to_owned()),
                        _ => ("pass", 0),
                    };
                    Ok(crate::process::Captured {
                        status: std::process::ExitStatus::from_raw(code << 8),
                        stdout: sim_campaign::tests::report_text(seed, verdict),
                        stderr: String::new(),
                    })
                },
            )
            .unwrap();
            validators.assert_matches(&report, &format!("a campaign: {case}"));
        }
    }

    #[test]
    fn demo_reports_match_their_schema() {
        let validators = Validators::load();
        let mut log = StepLog::new("demo m1", None, false).unwrap();
        let step = |log: &mut StepLog, name: &str, observed: Result<String, String>| {
            let step = steplog::Step {
                name,
                action: "an action",
                expected: "what it expects",
                rerun: "./dev demo m1 --assert",
            };
            log.record(step, observed, std::time::Duration::from_millis(5))
                .unwrap_or_else(|failed| *failed.record)
        };
        let passed = step(&mut log, "build", Ok("built".to_owned()));
        let failed = step(&mut log, "init", Err("refused".to_owned()));
        let report = |ok: bool, steps: Vec<steplog::StepRecord>| demo::DemoReport {
            schema: demo::SCHEMA,
            ok,
            label: demo::LABEL,
            milestone: "m1",
            assert: true,
            build: Some(demo::Build {
                id: format!("sha256:{}", "ab".repeat(32)),
                tree: "1".repeat(40),
                dirty: false,
            }),
            run_dir: ".local/demo/m1/run".to_owned(),
            steps,
            sim: Some(demo::SimPair {
                first: "34ec631c053276db".to_owned(),
                second: "34ec631c053276db".to_owned(),
            }),
            items: vec![demo::ItemStatus {
                id: "W18".to_owned(),
                title: "Production root".to_owned(),
                milestone: "m1".to_owned(),
                implemented: true,
            }],
            commands: vec!["./dev sim run --seed 42 --profile slice".to_owned()],
            limitations: demo::LIMITATIONS.to_vec(),
            step_log: ".local/demo/m1/run/steps.jsonl".to_owned(),
        };
        validators.assert_matches(&report(true, vec![passed.clone()]), "a passing demo");
        validators.assert_matches(
            &report(false, vec![passed.clone(), failed]),
            "a failed demo",
        );
        let mut early = report(false, vec![passed]);
        early.build = None;
        early.sim = None;
        early.items = Vec::new();
        validators.assert_matches(&early, "a demo that stopped before it identified the build");
    }

    #[test]
    fn cluster_reports_match_their_schema() {
        let validators = Validators::load();
        let node = |state, pid, detail: Option<&str>| cluster::NodeReport {
            node: 1,
            data_dir: ".local/nodes/1".to_owned(),
            state,
            pid,
            detail: detail.map(str::to_owned),
        };
        let report = |action, ok, nodes| cluster::ClusterReport {
            schema: cluster::SCHEMA,
            ok,
            action,
            config: "config/local.toml".to_owned(),
            state_dir: ".local/cluster/local-1a2b3c4d".to_owned(),
            instance_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            nodes,
            error: (!ok).then(|| "node 1 refused".to_owned()),
        };
        validators.assert_matches(
            &report(
                cluster::Action::Up,
                true,
                vec![node(cluster::NodeState::Started, Some(4242), None)],
            ),
            "cluster up",
        );
        validators.assert_matches(
            &report(
                cluster::Action::Status,
                true,
                vec![node(cluster::NodeState::Stale, None, Some("pid 7 is gone"))],
            ),
            "cluster status",
        );
        validators.assert_matches(
            &report(
                cluster::Action::Kill,
                true,
                vec![node(
                    cluster::NodeState::Killed,
                    None,
                    Some("pid 4242 killed"),
                )],
            ),
            "cluster kill",
        );
        validators.assert_matches(
            &report(
                cluster::Action::Restart,
                true,
                vec![node(cluster::NodeState::Started, Some(4343), None)],
            ),
            "cluster restart",
        );
        let mut refused = report(
            cluster::Action::Init,
            false,
            vec![node(cluster::NodeState::Refused, None, Some("used"))],
        );
        validators.assert_matches(&refused, "a refused init");
        refused.instance_id = None;
        validators.assert_matches(&refused, "a refused init before an ID exists");
    }

    #[test]
    fn smoke_reports_match_their_schema() {
        let validators = Validators::load();
        let mut log = StepLog::new("smoke lifecycle", None, false).unwrap();
        let mut step = |name: &str, observed: Result<String, String>| {
            let step = steplog::Step {
                name,
                action: "an action",
                expected: "what it expects",
                rerun: "./dev smoke lifecycle",
            };
            log.record(step, observed, std::time::Duration::from_millis(5))
                .unwrap_or_else(|failed| *failed.record)
        };
        let passed = step("leader", Ok("node 2 leads".to_owned()));
        let failed = step("catch-up", Err("node 4 applied 29".to_owned()));
        let report = |ok: bool, steps: Vec<steplog::StepRecord>, last_index: Option<u64>| {
            lifecycle::SmokeReport {
                schema: lifecycle::SCHEMA,
                ok,
                label: lifecycle::LABEL,
                run_dir: ".local/smoke/lifecycle/run".to_owned(),
                keys: lifecycle::KEYS,
                last_index,
                killed: vec![2, 1],
                steps,
                limitations: lifecycle::LIMITATIONS.to_vec(),
                step_log: ".local/smoke/lifecycle/run/steps.jsonl".to_owned(),
            }
        };
        validators.assert_matches(
            &report(true, vec![passed.clone()], Some(31)),
            "a passing smoke",
        );
        validators.assert_matches(
            &report(false, vec![passed, failed], Some(31)),
            "a failed smoke",
        );
        validators.assert_matches(
            &report(false, Vec::new(), None),
            "a smoke that stopped before any write",
        );
    }

    #[test]
    fn prune_reports_match_their_schema() {
        let validators = Validators::load();
        let root = scratch_dir("schemas-prune-root");
        let base = scratch_dir("schemas-prune-base");
        for tree in [
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
        ] {
            fs::create_dir_all(root.join(accept::ACCEPT_ROOT).join(tree)).unwrap();
        }
        fs::create_dir_all(base.join("keyval-checkout-0000000000000000")).unwrap();
        for dry_run in [true, false] {
            let layout = Layout {
                root: root.clone(),
                base: base.clone(),
                artifacts: scratch_dir("schemas-prune-artifacts"),
            };
            let report = prune::run(&layout, dry_run);
            assert!(
                !report.accept_trees.is_empty() && !report.target_dirs.is_empty(),
                "the layout gives prune something to judge; report={report:#?}"
            );
            let case = format!("prune::run dry_run={dry_run}");
            validators.assert_matches(&report, &case);
            if let Some(path) = report.receipt_path.as_deref() {
                validators.assert_matches(&read_json(Path::new(path)), &format!("{case}: receipt"));
            }
        }
    }

    #[test]
    fn schedule_reports_match_their_schema() {
        use crate::schedule::{Action, execute};
        use crate::schedule_plan::Job;
        let validators = Validators::load();
        let root = scratch_dir("schemas-schedule-root");
        let now = 20_000 * 86_400 + 5 * 3_600 + 30;
        let units = root.join("units");
        // A run that spawns the gate would race the lock tests of `schedule::tests` (a forked
        // child shares a held flock until it execs), so this sequence only queues: T2 waits
        // behind the owed T1, and a `running` record with no lock holder reads as incomplete.
        let mut interrupted = crate::schedule_plan::Record {
            state: crate::schedule_plan::State::Running,
            ..crate::schedule::new_record(Job::T1, now - 30, 1, now - 30)
        };
        interrupted.started_at = Some(now - 20);
        crate::schedule::save(&root.join(crate::schedule::DIR), &interrupted).unwrap();
        let actions = [
            Action::Status,
            Action::Install {
                apply: false,
                dir: Some(units),
            },
            Action::Run(Job::T2),
            Action::Status,
        ];
        for (n, action) in actions.iter().enumerate() {
            let report = execute(&root, action, now);
            validators.assert_matches(&report, &format!("schedule step {n}: {action:?}"));
        }
        let status = execute(&root, &Action::Status, now);
        assert!(
            status
                .jobs
                .iter()
                .any(|j| !j.recent.is_empty() && !j.missed.is_empty()),
            "the sequence leaves an incomplete record and missed slots for the schema to see; {status:#?}"
        );
    }

    #[test]
    fn coverage_reports_match_their_schema() {
        let validators = Validators::load();
        let root = scratch_dir("schemas-coverage-root");
        for file in [crate::coverage::RECORD_SCHEMA, crate::coverage::REQUIRED] {
            let to = root.join(file);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(workspace_root().join(file), to).unwrap();
        }
        let none = crate::coverage::run(&root, &[]).unwrap();
        validators.assert_matches(&none, "coverage::run without a record");
        let record = workspace_root().join("schemas/coverage/fixtures/campaign.v1.json");
        let some = crate::coverage::run(&root, &[record]).unwrap();
        assert!(
            !some.builds[0].empty_cells.is_empty(),
            "the fixture leaves cells empty, so the schema sees them; report={some:#?}"
        );
        validators.assert_matches(&some, "coverage::run on the campaign fixture");
    }

    #[test]
    fn traceability_reports_match_their_schema() {
        let validators = Validators::load();
        let artifacts = scratch_dir("schemas-traceability-artifacts");
        let receipts = artifacts.join("receipts");
        fs::create_dir_all(&receipts).unwrap();
        // The real registry, committed in a repository of its own, so the report reads it
        // from a judged tree as `./dev report traceability` does.
        let real = |path: &str| fs::read_to_string(workspace_root().join(path)).unwrap();
        let paths = [
            "registry/requirements.toml",
            "registry/checks.toml",
            "registry/workgraph.toml",
            ".beads/issues.jsonl",
        ];
        let texts: Vec<String> = paths.iter().map(|path| real(path)).collect();
        let files: Vec<(&str, &str)> = paths
            .iter()
            .copied()
            .zip(texts.iter().map(String::as_str))
            .collect();
        let git = fixture::repo("schemas-traceability-repo", &files);
        let judged = git.rev_parse("HEAD^{tree}").unwrap();
        // Real accept receipts, a full run and a `--case` run, so covering checks have
        // evidence of both kinds on the judged tree.
        let accept = json!({
            "schema": accept::SCHEMA, "ok": true, "tree": judged,
            "finished_at": "2026-10-03T22:00:00Z",
            "selectors": [
                { "selector": "./dev check DEV-SCHEMAS", "checks": ["DEV-SCHEMAS passed (18 cases)"] },
                { "selector": "./dev check XTASK --case schemas", "checks": ["XTASK passed (1 cases)"] },
            ],
        });
        fs::write(receipts.join("accept-x.json"), accept.to_string()).unwrap();
        let layout = traceability::Layout {
            work: scratch_dir("schemas-traceability-work"),
            artifacts,
            out: scratch_dir("schemas-traceability").join("traceability.md"),
        };
        let report = traceability::run(&git, &layout, Some(judged)).unwrap();
        assert_eq!(report.inputs, InputSource::Tree);
        validators.assert_matches(&report, "traceability::run on the real registry");
        // Without a tree (a git-isolated candidate), nothing is judged.
        let mut untreed = traceability::build(&traceability::Inputs {
            requirements: vec![traceability::RequirementRow {
                id: "O-6".to_owned(),
                level: "MUST".to_owned(),
                status: "active".to_owned(),
                title: "Robot mode".to_owned(),
                notes: None,
            }],
            checks: Vec::new(),
            items: Vec::new(),
            closed: BTreeSet::new(),
            evidence: Vec::new(),
            tree: None,
            registry: traceability::RegistryVerdict::default(),
            source: InputSource::WorkingTree,
        });
        untreed.ok = true;
        traceability::REPORT.clone_into(&mut untreed.path);
        validators.assert_matches(&untreed, "traceability::build without a tree");
    }

    #[test]
    fn step_records_match_their_schema() {
        let validators = Validators::load();
        let path = scratch_dir("schemas-steps").join("steps.jsonl");
        let mut log = StepLog::new("schemas", Some(&path), false).unwrap();
        let step = steplog::Step {
            name: "probe",
            action: "echo probe",
            expected: "exit 0",
            rerun: "./dev check DEV-SCHEMAS",
        };
        let elapsed = std::time::Duration::from_millis(7);
        let passed = log.record(step, Ok("exit 0".to_owned()), elapsed);
        let failed = log.record(step, Err("exit 1".to_owned()), elapsed);
        assert!(
            passed.is_ok() && failed.is_err(),
            "one step passed and one failed"
        );
        let lines = fs::read_to_string(&path).unwrap();
        assert_eq!(
            lines.lines().count(),
            2,
            "both records were written; log={lines}"
        );
        for line in lines.lines() {
            let record: Value = serde_json::from_str(line).unwrap();
            validators.assert_matches(&record, "StepLog::record");
        }
    }
}
