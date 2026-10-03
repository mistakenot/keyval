//! SPEC: `api/openapi.yaml` judged against PLAN.md §8 with the fixtures in `api/fixtures/`
//! (W03.3). `api/fixtures/README.md` states the rules and the fixture formats; this module
//! implements them and nothing else.
//!
//! One case per spec rule, plus one per positive, negative and mutant fixture. The rules
//! run first on the committed spec, which must break none of them. Then every instance
//! fixture is validated, and every mutant (an RFC 6902 patch of the spec) must break its
//! named rule. A mutant that breaks nothing would make that rule vacuous (P10).
//!
//! The `OpenAPI` 3.1 meta-schema is vendored in `xtask/assets/` and pinned by its SHA-256.
//! Validation is offline: the only external schema referenced is the JSON Schema 2020-12
//! meta-schema, which the validator bundles.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use jsonschema::{Draft, Keyword, Registry, ValidationError, Validator};
use saphyr::{LoadableYamlNode, Scalar, Yaml};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};

use crate::check::StaticOutcome;

/// The spec under judgement, relative to the workspace root.
const SPEC_PATH: &str = "api/openapi.yaml";
/// The fixture directory, relative to the workspace root.
const FIXTURES: &str = "api/fixtures";

/// The `OpenAPI` 3.1 schema, as published at
/// <https://spec.openapis.org/oas/3.1/schema/2022-10-07> (retrieved 2026-10-03).
const OAS_SCHEMA: &str = include_str!("../assets/oas-3.1-schema-2022-10-07.json");
/// SHA-256 of [`OAS_SCHEMA`]; a different file is never silently used.
const OAS_SCHEMA_SHA256: &str = "da01ba28852cac0de53893797cb8d1942bc3b05084f526dcc216717dec314ed0";

/// The URI the spec is registered under, so its `#/…` references resolve.
const SPEC_URI: &str = "urn:keyval:api:openapi";

/// The spec rules, in reporting order (README "Spec rules", less `instances`).
const RULES: [&str; 9] = [
    "openapi",
    "no-store",
    "examples",
    "closed",
    "codes",
    "integers",
    "operations",
    "limits",
    "plan-examples",
];

/// HTTP methods that name an Operation Object in a Path Item.
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];

/// Most `$ref` hops followed before a reference chain counts as a cycle.
const MAX_REF_HOPS: usize = 16;

/// Most validator messages quoted for one failure (CD-9).
const MAX_MESSAGES: usize = 3;

/// `contract.json`: PLAN.md §8 in machine-readable form.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contract {
    schema: String,
    /// Provenance, for readers; not checked.
    #[serde(rename = "source")]
    _source: String,
    openapi: String,
    cache_control: String,
    integer_maximum: u64,
    operations: Vec<ContractOperation>,
    codes: Vec<ContractCode>,
    byte_limits: Vec<ByteLimit>,
    examples: Vec<PlanExample>,
}

/// One §8.2 endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractOperation {
    operation: String,
    method: String,
    path: String,
    requirements: Vec<String>,
    parameters: Vec<ContractParameter>,
    request: Option<String>,
    success: String,
    statuses: Vec<String>,
}

/// One parameter of a contract operation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractParameter {
    name: String,
    #[serde(rename = "in")]
    location: String,
    required: bool,
}

/// One §8.4 error code and its HTTP status.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractCode {
    code: String,
    status: String,
}

/// One recorded byte limit.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ByteLimit {
    pointer: String,
    extension: String,
    value: u64,
    source: String,
}

/// Which side of an operation an example belongs to.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Side {
    Request,
    Response,
}

/// One §8.6 example, verbatim.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanExample {
    source: String,
    operation: String,
    #[serde(rename = "in")]
    side: Side,
    status: Option<String>,
    name: String,
    value: Value,
}

/// What a fixture file's cases must do.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Expect {
    Valid,
    Invalid,
}

/// `positive.json` or `negative.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixtures {
    schema: String,
    expect: Expect,
    cases: Vec<Case>,
}

/// One instance case.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    rule: String,
    target: Target,
    instance: Value,
}

/// The schema an instance is validated against.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Schema(String),
    Request(String),
    Response(ResponseTarget),
    Parameter(ParameterTarget),
}

/// A response schema: operation and status.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseTarget {
    operation: String,
    status: String,
}

/// A parameter schema: operation and parameter name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParameterTarget {
    operation: String,
    name: String,
}

/// `spec-mutants.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutants {
    schema: String,
    target: String,
    #[serde(rename = "mutants")]
    cases: Vec<Mutant>,
}

/// One mutated spec and the rule it must break.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutant {
    name: String,
    rule: String,
    /// Why the mutation must break the rule, for readers; not checked.
    #[serde(rename = "why")]
    _why: String,
    patch: Vec<PatchOp>,
}

/// The RFC 6902 operations the mutants use.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum PatchOp {
    Add { path: String, value: Value },
    Remove { path: String },
    Replace { path: String, value: Value },
}

/// Violations of the spec rules, keyed by rule.
type Findings = BTreeMap<&'static str, Vec<String>>;

// check: SPEC
/// Judges `<root>/api/openapi.yaml` against PLAN.md §8 with the fixtures in
/// `<root>/api/fixtures/` (rules in `api/fixtures/README.md`).
///
/// # Errors
///
/// Returns an error when a file cannot be read or parsed, or when a fixture file does not
/// have its expected schema identifier. Rule violations are not errors: they are reported
/// in the outcome.
pub fn check_spec(root: &Path) -> Result<StaticOutcome, String> {
    let spec = load_yaml(&root.join(SPEC_PATH))?;
    let contract: Contract = load_fixture(
        root,
        "contract.json",
        "keyval.spec.contract/v1",
        |c: &Contract| &c.schema,
    )?;
    let positive: Fixtures = load_fixture(
        root,
        "positive.json",
        "keyval.spec.fixtures/v1",
        |f: &Fixtures| &f.schema,
    )?;
    let negative: Fixtures = load_fixture(
        root,
        "negative.json",
        "keyval.spec.fixtures/v1",
        |f: &Fixtures| &f.schema,
    )?;
    let mutants: Mutants = load_fixture(
        root,
        "spec-mutants.json",
        "keyval.spec.mutants/v1",
        |m: &Mutants| &m.schema,
    )?;
    let oas = oas_validator()?;

    let mut violations = Vec::new();
    if positive.expect != Expect::Valid || negative.expect != Expect::Invalid {
        violations
            .push("positive.json must expect \"valid\" and negative.json \"invalid\"".to_owned());
    }
    if mutants.target != SPEC_PATH {
        violations.push(format!(
            "spec-mutants.json targets {:?}, not {SPEC_PATH:?}",
            mutants.target
        ));
    }
    let findings = judge(&spec, &contract, &oas);
    for (rule, found) in &findings {
        violations.extend(found.iter().map(|v| format!("[{rule}] {v}")));
    }
    if findings.get("openapi").is_none_or(Vec::is_empty) {
        for fixtures in [("positive.json", &positive), ("negative.json", &negative)] {
            violations.extend(check_instances(&spec, fixtures.0, fixtures.1)?);
        }
    } else {
        violations.push("[instances] skipped: the spec is not a valid OpenAPI document".to_owned());
    }
    violations.extend(check_mutants(&spec, &contract, &oas, &mutants));
    Ok(StaticOutcome {
        cases: RULES.len() + positive.cases.len() + negative.cases.len() + mutants.cases.len(),
        violations,
    })
}

/// Reads a YAML file holding one document as JSON.
fn load_yaml(path: &Path) -> Result<Value, String> {
    let text =
        fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let documents = Yaml::load_from_str(&text)
        .map_err(|err| format!("{} is not YAML: {err}", path.display()))?;
    match documents.as_slice() {
        [document] => yaml_to_json(document).map_err(|err| format!("{}: {err}", path.display())),
        other => Err(format!(
            "{} holds {} YAML documents; expected 1",
            path.display(),
            other.len()
        )),
    }
}

/// Converts a YAML node to JSON. Mapping keys must be strings or integers, and no two
/// keys may map to the same JSON member.
fn yaml_to_json(node: &Yaml<'_>) -> Result<Value, String> {
    match node {
        Yaml::Value(scalar) => scalar_to_json(scalar),
        Yaml::Sequence(items) => items
            .iter()
            .map(yaml_to_json)
            .collect::<Result<_, _>>()
            .map(Value::Array),
        Yaml::Mapping(entries) => {
            let mut object = Map::new();
            for (key, value) in entries {
                let key = match key {
                    Yaml::Value(Scalar::String(text)) => text.to_string(),
                    Yaml::Value(Scalar::Integer(number)) => number.to_string(),
                    other => return Err(format!("unsupported mapping key {other:?}")),
                };
                if object.insert(key.clone(), yaml_to_json(value)?).is_some() {
                    return Err(format!("duplicate mapping key {key:?}"));
                }
            }
            Ok(Value::Object(object))
        }
        Yaml::Representation(text, _, _) => Err(format!("unresolved scalar {text:?}")),
        Yaml::Tagged(tag, _) => Err(format!("unsupported tag {tag:?}")),
        Yaml::Alias(_) | Yaml::BadValue => Err("unresolved alias or invalid value".to_owned()),
    }
}

/// Converts a YAML core-schema scalar to JSON.
fn scalar_to_json(scalar: &Scalar<'_>) -> Result<Value, String> {
    Ok(match scalar {
        Scalar::Null => Value::Null,
        Scalar::Boolean(flag) => Value::Bool(*flag),
        Scalar::Integer(number) => Value::from(*number),
        Scalar::FloatingPoint(number) => {
            Number::from_f64(number.into_inner())
                .map(Value::Number)
                .ok_or_else(|| format!("{number} has no JSON representation"))?
        }
        Scalar::String(text) => Value::String(text.to_string()),
    })
}

/// Reads `api/fixtures/<name>` and checks its schema identifier.
fn load_fixture<T: DeserializeOwned>(
    root: &Path,
    name: &str,
    schema: &str,
    schema_of: impl Fn(&T) -> &String,
) -> Result<T, String> {
    let path = root.join(FIXTURES).join(name);
    let text = fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let parsed: T =
        serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    if schema_of(&parsed) != schema {
        return Err(format!(
            "{}: schema is {:?}; expected {schema:?}",
            path.display(),
            schema_of(&parsed)
        ));
    }
    Ok(parsed)
}

/// The `OpenAPI` 3.1 schema validator, after checking the vendored file's hash.
fn oas_validator() -> Result<Validator, String> {
    let digest = Sha256::digest(OAS_SCHEMA.as_bytes());
    let hex = digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    if hex != OAS_SCHEMA_SHA256 {
        return Err(format!(
            "xtask/assets/oas-3.1-schema-2022-10-07.json has SHA-256 {hex}; expected {OAS_SCHEMA_SHA256}"
        ));
    }
    let schema: Value = serde_json::from_str(OAS_SCHEMA)
        .map_err(|err| format!("vendored OpenAPI schema: {err}"))?;
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .map_err(|err| format!("vendored OpenAPI schema does not compile: {err}"))
}

/// Runs every spec rule on `doc`. When `openapi` fails, the other rules are skipped.
fn judge(doc: &Value, contract: &Contract, oas: &Validator) -> Findings {
    let mut findings: Findings = RULES.iter().map(|rule| (*rule, Vec::new())).collect();
    let openapi = rule_openapi(doc, contract, oas);
    if !openapi.is_empty() {
        findings.insert("openapi", openapi);
        return findings;
    }
    let registry = match Registry::new()
        .draft(Draft::Draft202012)
        .add(SPEC_URI, doc)
        .and_then(jsonschema::RegistryBuilder::prepare)
    {
        Ok(registry) => registry,
        Err(err) => {
            findings.insert(
                "openapi",
                vec![format!(
                    "the spec cannot be registered as a schema resource: {err}"
                )],
            );
            return findings;
        }
    };
    findings.insert("no-store", rule_no_store(doc, contract));
    findings.insert("examples", rule_examples(doc, &registry));
    findings.insert("closed", rule_closed(doc));
    findings.insert("codes", rule_codes(doc, contract));
    findings.insert("integers", rule_integers(doc, contract));
    findings.insert("operations", rule_operations(doc, contract));
    findings.insert("limits", rule_limits(doc, contract));
    findings.insert("plan-examples", rule_plan_examples(doc, contract));
    findings
}

/// `openapi`: the `OpenAPI` version, the 3.1 schema, and every local `$ref` resolving.
fn rule_openapi(doc: &Value, contract: &Contract, oas: &Validator) -> Vec<String> {
    let mut found = Vec::new();
    if doc.get("openapi").and_then(Value::as_str) != Some(contract.openapi.as_str()) {
        found.push(format!(
            "openapi is {}; expected {:?}",
            doc.get("openapi").unwrap_or(&Value::Null),
            contract.openapi
        ));
    }
    found.extend(oas.iter_errors(doc).take(MAX_MESSAGES).map(|err| {
        format!(
            "not a valid OpenAPI 3.1 document at {:?}: {err}",
            err.instance_path().to_string()
        )
    }));
    let mut refs = Vec::new();
    collect_refs(doc, &mut refs);
    for reference in refs {
        if pointer_of_ref(reference)
            .and_then(|pointer| doc.pointer(&pointer))
            .is_none()
        {
            found.push(format!("$ref {reference:?} does not resolve"));
        }
    }
    found
}

/// Every `$ref` string in `value`.
fn collect_refs<'v>(value: &'v Value, out: &mut Vec<&'v str>) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(reference)) = object.get("$ref") {
                out.push(reference);
            }
            object.values().for_each(|child| collect_refs(child, out));
        }
        Value::Array(items) => items.iter().for_each(|child| collect_refs(child, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// The JSON Pointer of a local reference (`#/…`), percent-decoded; `None` otherwise.
fn pointer_of_ref(reference: &str) -> Option<String> {
    let fragment = reference.strip_prefix('#')?;
    let bytes = fragment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let hex = fragment.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Escapes one JSON Pointer token (RFC 6901).
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Unescapes one JSON Pointer token (RFC 6901).
fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// Percent-encodes a JSON Pointer for use as a URI fragment (RFC 6901 §6).
fn fragment(pointer: &str) -> String {
    pointer
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Follows local `$ref`s from `pointer` to the object they lead to.
fn follow<'d>(doc: &'d Value, pointer: &str) -> Option<(String, &'d Value)> {
    let mut pointer = pointer.to_owned();
    for _ in 0..MAX_REF_HOPS {
        let value = doc.pointer(&pointer)?;
        match value.get("$ref").and_then(Value::as_str) {
            Some(reference) => pointer = pointer_of_ref(reference)?,
            None => return Some((pointer, value)),
        }
    }
    None
}

/// Every operation: (method, path, pointer to the Operation Object).
fn operations(doc: &Value) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    for (path, item) in doc
        .get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        for method in METHODS {
            if item.get(method).is_some() {
                out.push((
                    method,
                    path.clone(),
                    format!("/paths/{}/{method}", escape(path)),
                ));
            }
        }
    }
    out
}

/// The pointer of the operation with this `operationId`.
fn operation_pointer(doc: &Value, operation_id: &str) -> Option<String> {
    operations(doc)
        .into_iter()
        .map(|(_, _, pointer)| pointer)
        .find(|pointer| {
            doc.pointer(&format!("{pointer}/operationId"))
                .and_then(Value::as_str)
                == Some(operation_id)
        })
}

/// Every Response Object: (label, status if under an operation, pointer after `$ref`s).
fn responses(doc: &Value) -> Vec<(String, Option<String>, String)> {
    let mut out = Vec::new();
    for (method, path, op) in operations(doc) {
        for status in keys(doc, &format!("{op}/responses")) {
            if let Some((pointer, _)) = follow(doc, &format!("{op}/responses/{}", escape(&status)))
            {
                out.push((
                    format!("{} {path} {status}", method.to_uppercase()),
                    Some(status),
                    pointer,
                ));
            }
        }
    }
    for name in keys(doc, "/components/responses") {
        out.push((
            format!("components/responses/{name}"),
            None,
            format!("/components/responses/{}", escape(&name)),
        ));
    }
    out
}

/// The member names of the object at `pointer`, if it is one.
fn keys(doc: &Value, pointer: &str) -> Vec<String> {
    doc.pointer(pointer)
        .and_then(Value::as_object)
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

/// Every Media Type Object reachable from an operation: (label, pointer), deduplicated.
fn media_types(doc: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut holders = Vec::new();
    for (method, path, op) in operations(doc) {
        if let Some((pointer, _)) = follow(doc, &format!("{op}/requestBody")) {
            holders.push((format!("{} {path} request", method.to_uppercase()), pointer));
        }
    }
    holders.extend(
        responses(doc)
            .into_iter()
            .filter(|(_, status, _)| status.is_some())
            .map(|(label, _, p)| (label, p)),
    );
    for (label, holder) in holders {
        for media in keys(doc, &format!("{holder}/content")) {
            let pointer = format!("{holder}/content/{}", escape(&media));
            if !out.iter().any(|(_, seen)| *seen == pointer) {
                out.push((format!("{label} {media}"), pointer));
            }
        }
    }
    out
}

/// `no-store`: every response declares a required `Cache-Control: no-store` header.
fn rule_no_store(doc: &Value, contract: &Contract) -> Vec<String> {
    let mut found = Vec::new();
    for (label, _, pointer) in responses(doc) {
        let header =
            follow(doc, &format!("{pointer}/headers/Cache-Control")).map(|(_, header)| header);
        let ok = header.is_some_and(|header| {
            header.get("required") == Some(&Value::Bool(true))
                && header.pointer("/schema/type").and_then(Value::as_str) == Some("string")
                && header.pointer("/schema/const").and_then(Value::as_str)
                    == Some(contract.cache_control.as_str())
        });
        if !ok {
            found.push(format!(
                "{label}: no required Cache-Control: {} header",
                contract.cache_control
            ));
        }
    }
    found
}

/// `examples`: every Example Object validates against its media type's schema.
fn rule_examples(doc: &Value, registry: &Registry<'_>) -> Vec<String> {
    let mut found = Vec::new();
    for (label, media) in media_types(doc) {
        let names = keys(doc, &format!("{media}/examples"));
        if names.is_empty() {
            continue;
        }
        let validator = match instance_validator(registry, &format!("{media}/schema")) {
            Ok(validator) => validator,
            Err(err) => {
                found.push(format!("{label}: {err}"));
                continue;
            }
        };
        for name in names {
            match doc.pointer(&format!("{media}/examples/{}/value", escape(&name))) {
                Some(value) => {
                    if let Some(why) = first_errors(&validator, value) {
                        found.push(format!("{label} example {name:?}: {why}"));
                    }
                }
                None => found.push(format!("{label} example {name:?} has no inline value")),
            }
        }
    }
    found
}

/// The roots of every schema the spec defines, with their pointers.
fn schema_roots(doc: &Value) -> Vec<String> {
    let mut roots: Vec<String> = keys(doc, "/components/schemas")
        .iter()
        .map(|name| format!("/components/schemas/{}", escape(name)))
        .collect();
    for kind in ["parameters", "headers"] {
        for name in keys(doc, &format!("/components/{kind}")) {
            roots.push(format!("/components/{kind}/{}/schema", escape(&name)));
        }
    }
    roots.extend(
        media_types(doc)
            .into_iter()
            .map(|(_, media)| format!("{media}/schema")),
    );
    roots.retain(|pointer| doc.pointer(pointer).is_some());
    roots
}

/// Every schema object under `pointer` (itself and its subschemas), as (pointer, schema).
fn subschemas<'d>(doc: &'d Value, pointer: &str, out: &mut Vec<(String, &'d Map<String, Value>)>) {
    let Some(Value::Object(schema)) = doc.pointer(pointer) else {
        return;
    };
    out.push((pointer.to_owned(), schema));
    for keyword in ["properties", "patternProperties", "$defs"] {
        for name in keys(doc, &format!("{pointer}/{keyword}")) {
            subschemas(doc, &format!("{pointer}/{keyword}/{}", escape(&name)), out);
        }
    }
    for keyword in [
        "items",
        "not",
        "if",
        "then",
        "else",
        "additionalProperties",
        "contains",
    ] {
        subschemas(doc, &format!("{pointer}/{keyword}"), out);
    }
    for keyword in ["oneOf", "anyOf", "allOf", "prefixItems"] {
        let count = schema
            .get(keyword)
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for index in 0..count {
            subschemas(doc, &format!("{pointer}/{keyword}/{index}"), out);
        }
    }
}

/// Every schema object the spec defines, as (pointer, schema).
fn all_schemas(doc: &Value) -> Vec<(String, &Map<String, Value>)> {
    let mut out = Vec::new();
    for root in schema_roots(doc) {
        subschemas(doc, &root, &mut out);
    }
    out
}

/// `closed`: every `type: object` schema sets `additionalProperties: false`.
fn rule_closed(doc: &Value) -> Vec<String> {
    all_schemas(doc)
        .into_iter()
        .filter(|(_, schema)| schema.get("type").and_then(Value::as_str) == Some("object"))
        .filter(|(_, schema)| schema.get("additionalProperties") != Some(&Value::Bool(false)))
        .map(|(pointer, _)| {
            format!("{pointer}: object schema is not closed (additionalProperties: false)")
        })
        .collect()
}

/// `integers`: every integer schema declares a maximum within the contract's ceiling.
fn rule_integers(doc: &Value, contract: &Contract) -> Vec<String> {
    let is_integer = |schema: &Map<String, Value>| match schema.get("type") {
        Some(Value::String(kind)) => kind == "integer",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "integer"),
        _ => false,
    };
    all_schemas(doc)
        .into_iter()
        .filter(|(_, schema)| is_integer(schema))
        .filter(|(_, schema)| {
            schema
                .get("maximum")
                .and_then(Value::as_u64)
                .is_none_or(|max| max > contract.integer_maximum)
        })
        .map(|(pointer, schema)| {
            format!(
                "{pointer}: integer maximum {} is not at most {}",
                schema.get("maximum").unwrap_or(&Value::Null),
                contract.integer_maximum
            )
        })
        .collect()
}

/// The error schemas: component name → its `code` const, and whether `code` references
/// `ErrorCode`.
fn error_schemas(doc: &Value) -> BTreeMap<String, (Option<String>, bool)> {
    let mut out = BTreeMap::new();
    for name in keys(doc, "/components/schemas") {
        let code = format!(
            "/components/schemas/{}/properties/error/properties/code",
            escape(&name)
        );
        if doc
            .pointer(&format!(
                "/components/schemas/{}/properties/error",
                escape(&name)
            ))
            .is_some()
        {
            let constant = doc
                .pointer(&format!("{code}/const"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let tied = doc.pointer(&format!("{code}/$ref")).and_then(Value::as_str)
                == Some("#/components/schemas/ErrorCode");
            out.insert(
                format!("#/components/schemas/{}", escape(&name)),
                (constant, tied),
            );
        }
    }
    out
}

/// `codes`: the twelve codes, each error schema tied to `ErrorCode`, each code under its
/// own status, and each (code, status) reachable from an operation.
fn rule_codes(doc: &Value, contract: &Contract) -> Vec<String> {
    let mut found = Vec::new();
    let want: BTreeMap<&str, &str> = contract
        .codes
        .iter()
        .map(|c| (c.code.as_str(), c.status.as_str()))
        .collect();
    let enumerated: Vec<&str> = doc
        .pointer("/components/schemas/ErrorCode/enum")
        .and_then(Value::as_array)
        .map(|codes| codes.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut sorted = enumerated.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != enumerated.len() || sorted != want.keys().copied().collect::<Vec<_>>() {
        found.push(format!(
            "ErrorCode enum {enumerated:?} is not exactly the §8.4 codes {:?}",
            want.keys().collect::<Vec<_>>()
        ));
    }
    let schemas = error_schemas(doc);
    for (name, (constant, tied)) in &schemas {
        if !tied {
            found.push(format!(
                "{name}: code does not reference #/components/schemas/ErrorCode"
            ));
        }
        if !constant.as_deref().is_some_and(|c| enumerated.contains(&c)) {
            found.push(format!(
                "{name}: code const {constant:?} is not in ErrorCode"
            ));
        }
    }
    let mut reached = Vec::new();
    for (label, status, pointer) in responses(doc) {
        let Some(status) = status else { continue };
        for media in keys(doc, &format!("{pointer}/content")) {
            let mut stack = vec![format!("{pointer}/content/{}/schema", escape(&media))];
            while let Some(at) = stack.pop() {
                let Some(schema) = doc.pointer(&at) else {
                    continue;
                };
                if let Some((Some(code), _)) = schema
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| schemas.get(r))
                {
                    reached.push((code.clone(), status.clone()));
                    if want.get(code.as_str()) != Some(&status.as_str()) {
                        found.push(format!(
                            "{label}: code {code:?} belongs under {:?}",
                            want.get(code.as_str())
                        ));
                    }
                }
                for keyword in ["oneOf", "anyOf"] {
                    let count = schema
                        .get(keyword)
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    stack.extend((0..count).map(|index| format!("{at}/{keyword}/{index}")));
                }
            }
        }
    }
    for (code, status) in &want {
        if !reached.iter().any(|(c, s)| c == code && s == status) {
            found.push(format!(
                "code {code:?} ({status}) is reachable from no operation"
            ));
        }
    }
    found
}

/// The parameters of an operation, path-level included, by name: (in, required, pointer).
fn parameters(doc: &Value, op: &str) -> BTreeMap<String, (String, bool, String)> {
    let mut out = BTreeMap::new();
    let path_item = op.rsplit_once('/').map_or("", |(item, _)| item);
    for holder in [path_item, op] {
        let count = doc
            .pointer(&format!("{holder}/parameters"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for index in 0..count {
            if let Some((pointer, parameter)) = follow(doc, &format!("{holder}/parameters/{index}"))
            {
                let text = |field: &str| {
                    parameter
                        .get(field)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                let required = parameter.get("required") == Some(&Value::Bool(true));
                out.insert(text("name"), (text("in"), required, pointer));
            }
        }
    }
    out
}

/// The component schema name a `$ref`-only schema points at, if any.
fn schema_name(schema: Option<&Value>) -> Option<String> {
    schema?
        .get("$ref")?
        .as_str()?
        .strip_prefix("#/components/schemas/")
        .map(unescape)
}

/// `operations`: the spec's operations are exactly the contract's, field by field.
fn rule_operations(doc: &Value, contract: &Contract) -> Vec<String> {
    let mut found = Vec::new();
    let have = operations(doc);
    for (method, path, _) in &have {
        if !contract
            .operations
            .iter()
            .any(|o| o.method == *method && o.path == *path)
        {
            found.push(format!(
                "{} {path} is not in the contract",
                method.to_uppercase()
            ));
        }
    }
    for want in &contract.operations {
        let Some((_, _, op)) = have
            .iter()
            .find(|(m, p, _)| *m == want.method && *p == want.path)
        else {
            found.push(format!(
                "{}: {} {} is missing",
                want.operation,
                want.method.to_uppercase(),
                want.path
            ));
            continue;
        };
        found.extend(
            compare_operation(doc, op, want)
                .into_iter()
                .map(|why| format!("{}: {why}", want.operation)),
        );
    }
    found
}

/// The differences between one operation and its contract entry.
fn compare_operation(doc: &Value, op: &str, want: &ContractOperation) -> Vec<String> {
    let mut found = Vec::new();
    let field = |name: &str| {
        doc.pointer(&format!("{op}/{name}"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    if field("operationId") != Value::String(want.operation.clone()) {
        found.push(format!("operationId is {}", field("operationId")));
    }
    if field("x-requirements") != json!(want.requirements) {
        found.push(format!(
            "x-requirements {} != {:?}",
            field("x-requirements"),
            want.requirements
        ));
    }
    let have = parameters(doc, op);
    let names: Vec<&String> = have.keys().collect();
    let mut wanted: Vec<&String> = want.parameters.iter().map(|p| &p.name).collect();
    wanted.sort_unstable();
    if names != wanted {
        found.push(format!("parameters {names:?} != {wanted:?}"));
    }
    for parameter in &want.parameters {
        if let Some((location, required, _)) = have.get(&parameter.name)
            && (*location != parameter.location || *required != parameter.required)
        {
            found.push(format!(
                "parameter {} is in={location} required={required}; expected in={} required={}",
                parameter.name, parameter.location, parameter.required
            ));
        }
    }
    let body = follow(doc, &format!("{op}/requestBody"));
    let request = body
        .as_ref()
        .and_then(|(_, body)| schema_name(body.pointer("/content/application~1json/schema")));
    if request != want.request {
        found.push(format!(
            "request body schema {request:?} != {:?}",
            want.request
        ));
    }
    if body.is_some_and(|(_, body)| body.get("required") != Some(&Value::Bool(true))) {
        found.push("request body is not required".to_owned());
    }
    let mut statuses = keys(doc, &format!("{op}/responses"));
    statuses.sort_unstable();
    let mut wanted_statuses = want.statuses.clone();
    wanted_statuses.sort_unstable();
    if statuses != wanted_statuses {
        found.push(format!("statuses {statuses:?} != {wanted_statuses:?}"));
    }
    let success = follow(doc, &format!("{op}/responses/200")).and_then(|(_, response)| {
        schema_name(response.pointer("/content/application~1json/schema"))
    });
    if success.as_deref() != Some(want.success.as_str()) {
        found.push(format!("200 schema {success:?} != {:?}", want.success));
    }
    found
}

/// `limits`: each recorded byte limit exists with exactly its value.
fn rule_limits(doc: &Value, contract: &Contract) -> Vec<String> {
    contract
        .byte_limits
        .iter()
        .filter(|limit| {
            doc.pointer(&limit.pointer)
                .and_then(|holder| holder.get(&limit.extension))
                .and_then(Value::as_u64)
                != Some(limit.value)
        })
        .map(|limit| {
            format!(
                "{} {} is not {} ({})",
                limit.pointer, limit.extension, limit.value, limit.source
            )
        })
        .collect()
}

/// `plan-examples`: each §8.6 example is present and equal to the contract's copy.
fn rule_plan_examples(doc: &Value, contract: &Contract) -> Vec<String> {
    let mut found = Vec::new();
    for example in &contract.examples {
        let holder = operation_pointer(doc, &example.operation).and_then(|op| {
            match (example.side, &example.status) {
                (Side::Request, _) => follow(doc, &format!("{op}/requestBody")),
                (Side::Response, Some(status)) => {
                    follow(doc, &format!("{op}/responses/{}", escape(status)))
                }
                (Side::Response, None) => None,
            }
        });
        let value = holder.and_then(|(_, holder)| {
            holder.pointer(&format!(
                "/content/application~1json/examples/{}/value",
                escape(&example.name)
            ))
        });
        if value != Some(&example.value) {
            found.push(format!(
                "{}: {} {:?} {} example {:?} is missing or differs",
                example.source,
                example.operation,
                example.side,
                example.status.as_deref().unwrap_or(""),
                example.name
            ));
        }
    }
    found
}

/// The pointer of the schema a case's target names.
fn target_pointer(doc: &Value, target: &Target) -> Result<String, String> {
    let op = |id: &str| operation_pointer(doc, id).ok_or_else(|| format!("no operation {id:?}"));
    let pointer = match target {
        Target::Schema(name) => format!("/components/schemas/{}", escape(name)),
        Target::Request(id) => {
            let (body, _) = follow(doc, &format!("{}/requestBody", op(id)?))
                .ok_or_else(|| format!("{id} has no request body"))?;
            format!("{body}/content/application~1json/schema")
        }
        Target::Response(ResponseTarget { operation, status }) => {
            let at = format!("{}/responses/{}", op(operation)?, escape(status));
            let (response, _) =
                follow(doc, &at).ok_or_else(|| format!("{operation} has no {status} response"))?;
            format!("{response}/content/application~1json/schema")
        }
        Target::Parameter(ParameterTarget { operation, name }) => {
            let all = parameters(doc, &op(operation)?);
            let (_, _, parameter) = all
                .get(name)
                .ok_or_else(|| format!("{operation} has no parameter {name:?}"))?;
            format!("{parameter}/schema")
        }
    };
    if doc.pointer(&pointer).is_none() {
        return Err(format!("{pointer} does not exist"));
    }
    Ok(pointer)
}

/// A validator for the schema at `pointer` in the registered spec, with the extension
/// keywords of README "Extension keywords".
fn instance_validator(registry: &Registry<'_>, pointer: &str) -> Result<Validator, String> {
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_registry(registry)
        .should_validate_formats(true)
        .with_keyword("x-maxUtf8Bytes", |_, value, _| {
            byte_limit(value)
                .map(|limit| Box::new(MaxUtf8Bytes(limit)) as Box<dyn for<'i> Keyword<'i>>)
        })
        .with_keyword("x-maxPageUtf8Bytes", |_, value, _| {
            byte_limit(value)
                .map(|limit| Box::new(MaxPageUtf8Bytes(limit)) as Box<dyn for<'i> Keyword<'i>>)
        })
        .build(&json!({ "$ref": format!("{SPEC_URI}#{}", fragment(pointer)) }))
        .map_err(|err| format!("schema {pointer} does not compile: {err}"))
}

/// An extension keyword's limit: a non-negative integer.
fn byte_limit(value: &Value) -> Result<usize, ValidationError<'static>> {
    value
        .as_u64()
        .and_then(|limit| usize::try_from(limit).ok())
        .ok_or_else(|| {
            ValidationError::custom(format!(
                "a byte limit must be a non-negative integer, not {value}"
            ))
        })
}

/// `x-maxUtf8Bytes`: a string's UTF-8 encoding is at most this many bytes.
struct MaxUtf8Bytes(usize);

impl<'i> Keyword<'i> for MaxUtf8Bytes {
    fn validate(&self, instance: &'i Value) -> Result<(), ValidationError<'i>> {
        match instance.as_str() {
            Some(text) if text.len() > self.0 => Err(ValidationError::custom(format!(
                "{} UTF-8 bytes exceed x-maxUtf8Bytes {}",
                text.len(),
                self.0
            ))),
            Some(_) | None => Ok(()),
        }
    }

    fn is_valid(&self, instance: &'i Value) -> bool {
        instance.as_str().is_none_or(|text| text.len() <= self.0)
    }
}

/// `x-maxPageUtf8Bytes`: a page of more than one item holds at most this many decoded
/// key and value bytes (the first item is always returned, §7.11).
struct MaxPageUtf8Bytes(usize);

impl MaxPageUtf8Bytes {
    /// The page's decoded key+value bytes, when it holds more than one item.
    fn bytes(instance: &Value) -> Option<usize> {
        let items = instance
            .get("items")?
            .as_array()
            .filter(|items| items.len() > 1)?;
        let text =
            |item: &Value, field: &str| item.get(field).and_then(Value::as_str).map_or(0, str::len);
        Some(
            items
                .iter()
                .map(|item| text(item, "key") + text(item, "value"))
                .sum(),
        )
    }
}

impl<'i> Keyword<'i> for MaxPageUtf8Bytes {
    fn validate(&self, instance: &'i Value) -> Result<(), ValidationError<'i>> {
        match Self::bytes(instance) {
            Some(total) if total > self.0 => Err(ValidationError::custom(format!(
                "the page holds {total} decoded bytes; x-maxPageUtf8Bytes is {}",
                self.0
            ))),
            Some(_) | None => Ok(()),
        }
    }

    fn is_valid(&self, instance: &'i Value) -> bool {
        Self::bytes(instance).is_none_or(|total| total <= self.0)
    }
}

/// The first few validation errors, or `None` when the instance is valid.
fn first_errors(validator: &Validator, instance: &Value) -> Option<String> {
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .take(MAX_MESSAGES)
        .map(|err| format!("at {:?}: {err}", err.instance_path().to_string()))
        .collect();
    (!errors.is_empty()).then(|| errors.join("; "))
}

/// Replaces every `{"$repeat": s, "times": n}` with `s` repeated `n` times.
fn expand(value: &Value) -> Result<Value, String> {
    Ok(match value {
        Value::Object(object)
            if object.len() == 2
                && object.contains_key("$repeat")
                && object.contains_key("times") =>
        {
            let text = object.get("$repeat").and_then(Value::as_str);
            let times = object
                .get("times")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
            match (text, times) {
                (Some(text), Some(times)) => Value::String(text.repeat(times)),
                _ => return Err(format!("malformed $repeat {value}")),
            }
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(k, v)| Ok((k.clone(), expand(v)?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Array(items) => Value::Array(items.iter().map(expand).collect::<Result<_, _>>()?),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    })
}

/// `instances`: each case of one fixture file validates (positive) or does not (negative).
fn check_instances(spec: &Value, file: &str, fixtures: &Fixtures) -> Result<Vec<String>, String> {
    let registry = Registry::new()
        .draft(Draft::Draft202012)
        .add(SPEC_URI, spec)
        .and_then(jsonschema::RegistryBuilder::prepare)
        .map_err(|err| format!("the spec cannot be registered as a schema resource: {err}"))?;
    let mut names: Vec<&str> = fixtures
        .cases
        .iter()
        .map(|case| case.name.as_str())
        .collect();
    names.sort_unstable();
    let mut found: Vec<String> = names
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .map(|pair| format!("[instances] {file}: duplicate case {:?}", pair[0]))
        .collect();
    for case in &fixtures.cases {
        let outcome = target_pointer(spec, &case.target)
            .and_then(|pointer| instance_validator(&registry, &pointer))
            .and_then(|validator| Ok(first_errors(&validator, &expand(&case.instance)?)));
        let problem = match (outcome, fixtures.expect) {
            (Err(err), _) => Some(err),
            (Ok(Some(why)), Expect::Valid) => Some(format!("expected valid, got invalid {why}")),
            (Ok(None), Expect::Invalid) => Some("expected invalid, got valid".to_owned()),
            (Ok(Some(_)), Expect::Invalid) | (Ok(None), Expect::Valid) => None,
        };
        if let Some(problem) = problem {
            found.push(format!(
                "[instances] {file} case {}: {problem} (rule: {})",
                case.name, case.rule
            ));
        }
    }
    Ok(found)
}

/// Applies one RFC 6902 operation to `doc`.
fn apply(doc: &mut Value, op: &PatchOp) -> Result<(), String> {
    let path = match op {
        PatchOp::Add { path, .. } | PatchOp::Remove { path } | PatchOp::Replace { path, .. } => {
            path
        }
    };
    let (parent, last) = path
        .rsplit_once('/')
        .ok_or_else(|| format!("{path:?} is not a JSON Pointer"))?;
    let token = unescape(last);
    let missing = || format!("{path} does not exist");
    let holder = doc.pointer_mut(parent).ok_or_else(missing)?;
    let index = |len: usize| {
        token
            .parse::<usize>()
            .ok()
            .filter(|index| *index < len)
            .ok_or_else(missing)
    };
    match (op, holder) {
        (PatchOp::Add { value, .. }, Value::Object(object)) => {
            object.insert(token, value.clone());
        }
        (PatchOp::Add { value, .. }, Value::Array(items)) if token == "-" => {
            items.push(value.clone());
        }
        (PatchOp::Add { value, .. }, Value::Array(items)) => {
            let at = token
                .parse::<usize>()
                .ok()
                .filter(|at| *at <= items.len())
                .ok_or_else(missing)?;
            items.insert(at, value.clone());
        }
        (PatchOp::Remove { .. }, Value::Object(object)) => {
            object.remove(&token).ok_or_else(missing)?;
        }
        (PatchOp::Remove { .. }, Value::Array(items)) => {
            items.remove(index(items.len())?);
        }
        (PatchOp::Replace { value, .. }, Value::Object(object)) => {
            *object.get_mut(&token).ok_or_else(missing)? = value.clone();
        }
        (PatchOp::Replace { value, .. }, Value::Array(items)) => {
            let at = index(items.len())?;
            if let Some(slot) = items.get_mut(at) {
                *slot = value.clone();
            }
        }
        (_, Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            return Err(format!("{parent} is not a container"));
        }
    }
    Ok(())
}

/// `mutants`: each patched spec breaks its named rule.
fn check_mutants(
    spec: &Value,
    contract: &Contract,
    oas: &Validator,
    mutants: &Mutants,
) -> Vec<String> {
    let mut found = Vec::new();
    for mutant in &mutants.cases {
        if !RULES.contains(&mutant.rule.as_str()) {
            found.push(format!(
                "[mutants] {}: unknown rule {:?}",
                mutant.name, mutant.rule
            ));
            continue;
        }
        let mut doc = spec.clone();
        if let Err(err) = mutant.patch.iter().try_for_each(|op| apply(&mut doc, op)) {
            found.push(format!(
                "[mutants] {}: the patch does not apply: {err}",
                mutant.name
            ));
            continue;
        }
        let findings = judge(&doc, contract, oas);
        if findings.get(mutant.rule.as_str()).is_none_or(Vec::is_empty) {
            let fired: Vec<&&str> = findings
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(rule, _)| rule)
                .collect();
            found.push(format!(
                "[mutants] {}: rule {:?} did not fire (fired: {fired:?})",
                mutant.name, mutant.rule
            ));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Value {
        let documents = Yaml::load_from_str(text).unwrap();
        yaml_to_json(&documents[0]).unwrap()
    }

    #[test]
    fn yaml_scalars_keys_and_nesting_convert_to_json() {
        let converted = yaml(
            "a: '200'\n200: x\nn: 9007199254740991\nv: 3.1.0\nf: 1.5\nt: true\nz: ~\nl: [no-store, 'null']\n",
        );
        let expected = json!({
            "a": "200", "200": "x", "n": 9_007_199_254_740_991_u64, "v": "3.1.0", "f": 1.5,
            "t": true, "z": null, "l": ["no-store", "null"]
        });
        assert_eq!(
            converted, expected,
            "rule: YAML 1.2 core schema maps onto JSON; input as shown"
        );
    }

    #[test]
    fn yaml_keys_that_collide_in_json_are_rejected() {
        let documents = Yaml::load_from_str("200: a\n'200': b\n").unwrap();
        let result = yaml_to_json(&documents[0]);
        assert!(
            result.is_err(),
            "rule: 200 and '200' are one JSON member; observed={result:?}"
        );
    }

    #[test]
    fn repeat_objects_expand_everywhere_and_malformed_ones_fail() {
        let expanded = expand(
            &json!({"v": {"$repeat": "é", "times": 3}, "l": [{"$repeat": "ab", "times": 2}]}),
        )
        .unwrap();
        assert_eq!(
            expanded,
            json!({"v": "ééé", "l": ["abab"]}),
            "rule: README $repeat expansion"
        );
        let bad = expand(&json!({"$repeat": 1, "times": 2}));
        assert!(
            bad.is_err(),
            "rule: $repeat needs a string; observed={bad:?}"
        );
    }

    #[test]
    fn patches_add_remove_and_replace_with_escaped_tokens() {
        let mut doc = json!({"paths": {"/v1/kv": {"get": 1}}, "list": ["a", "b"]});
        let ops: Vec<PatchOp> = serde_json::from_value(json!([
            {"op": "replace", "path": "/paths/~1v1~1kv/get", "value": 2},
            {"op": "add", "path": "/list/-", "value": "c"},
            {"op": "remove", "path": "/list/0"},
            {"op": "add", "path": "/new", "value": true}
        ]))
        .unwrap();
        for op in &ops {
            apply(&mut doc, op).unwrap();
        }
        assert_eq!(
            doc,
            json!({"paths": {"/v1/kv": {"get": 2}}, "list": ["b", "c"], "new": true}),
            "rule: RFC 6902"
        );
        let missing: PatchOp =
            serde_json::from_value(json!({"op": "remove", "path": "/absent"})).unwrap();
        assert!(
            apply(&mut doc, &missing).is_err(),
            "rule: removing a missing member fails"
        );
    }

    #[test]
    fn pointers_round_trip_through_uri_fragments() {
        let pointer = "/paths/~1v1~1requests~1{request_id}/get";
        let encoded = fragment(pointer);
        assert_eq!(
            encoded, "/paths/~1v1~1requests~1%7Brequest_id%7D/get",
            "rule: RFC 6901 §6"
        );
        assert_eq!(
            pointer_of_ref(&format!("#{encoded}")).as_deref(),
            Some(pointer),
            "rule: decoding inverts encoding"
        );
        assert_eq!(
            pointer_of_ref("other.yaml#/x"),
            None,
            "rule: only local references resolve"
        );
    }

    #[test]
    fn extension_keywords_count_utf8_bytes_not_characters() {
        let short = MaxUtf8Bytes(4);
        assert!(
            short.is_valid(&json!("éé")),
            "rule: two 2-byte characters are 4 bytes"
        );
        assert!(
            !short.is_valid(&json!("ééé")),
            "rule: three 2-byte characters are 6 bytes"
        );
        assert!(
            short.validate(&json!("ééé")).is_err(),
            "rule: validate agrees with is_valid"
        );
        let page = MaxPageUtf8Bytes(4);
        let one = json!({"items": [{"key": "kk", "value": "vvvvv"}]});
        let two = json!({"items": [{"key": "k", "value": "v"}, {"key": "k", "value": "vv"}]});
        assert!(
            page.is_valid(&one),
            "rule: the first item is always returned (§7.11)"
        );
        assert!(
            !page.is_valid(&two),
            "rule: 5 bytes over two items exceed 4"
        );
    }

    #[test]
    fn the_vendored_openapi_schema_rejects_a_document_without_info() {
        let oas = oas_validator().unwrap();
        let minimal =
            json!({"openapi": "3.1.0", "info": {"title": "t", "version": "1"}, "paths": {}});
        assert!(
            oas.is_valid(&minimal),
            "rule: a minimal OAS 3.1 document is valid; doc={minimal}"
        );
        assert!(
            !oas.is_valid(&json!({"openapi": "3.1.0", "paths": {}})),
            "rule: info is required (OAS 3.1)"
        );
    }

    #[test]
    fn the_committed_spec_meets_every_rule_and_fixture() {
        let root = crate::test_support::workspace_root();
        let outcome = check_spec(&root).unwrap();
        assert!(
            outcome.violations.is_empty(),
            "rule: api/openapi.yaml meets PLAN.md §8 per api/fixtures (W03.3); rerun: ./dev check SPEC; \
             violations={:#?}",
            outcome.violations
        );
        assert!(
            outcome.cases >= RULES.len() + 3,
            "rule: the selection is not vacuous; cases={}",
            outcome.cases
        );
    }
}
