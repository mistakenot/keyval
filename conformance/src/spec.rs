//! The parts of `api/openapi.yaml` the harness needs: operation IDs and error codes.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Path;

use saphyr::{LoadableYamlNode, Scalar, Yaml};
use serde_json::Value;

const METHODS: [&str; 5] = ["get", "put", "post", "delete", "patch"];

/// Why the spec could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum SpecError {
    /// The file could not be read.
    Unreadable(String),
    /// The text is not exactly one YAML document with plain keys.
    NotYaml(String),
    /// The document lacks `paths` operations or `components.schemas.ErrorCode.enum`.
    Incomplete(String),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(why) => write!(f, "cannot read the spec: {why}"),
            Self::NotYaml(why) => write!(f, "the spec is not usable YAML: {why}"),
            Self::Incomplete(why) => write!(f, "the spec is incomplete: {why}"),
        }
    }
}

/// One operation of the spec.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Operation {
    /// Its `operationId`.
    pub id: String,
    /// Lower-case HTTP method.
    pub method: String,
    /// Path template, e.g. `/v1/keys/{key}`.
    pub path: String,
}

/// The operations and error codes a test may claim to exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    operations: Vec<Operation>,
    error_codes: BTreeSet<String>,
}

impl Spec {
    /// Reads and parses the spec file.
    ///
    /// # Errors
    /// [`SpecError`] when the file is unreadable, is not one YAML document, or lacks
    /// operations or the `ErrorCode` enum.
    pub fn load(path: &Path) -> Result<Self, SpecError> {
        let text = fs::read_to_string(path)
            .map_err(|err| SpecError::Unreadable(format!("{}: {err}", path.display())))?;
        Self::parse(&text)
    }

    /// Parses spec text.
    ///
    /// # Errors
    /// As [`Spec::load`], without the file error.
    pub fn parse(text: &str) -> Result<Self, SpecError> {
        let documents =
            Yaml::load_from_str(text).map_err(|err| SpecError::NotYaml(err.to_string()))?;
        let [document] = documents.as_slice() else {
            return Err(SpecError::NotYaml(format!(
                "{} documents; expected 1",
                documents.len()
            )));
        };
        let json = to_json(document).map_err(SpecError::NotYaml)?;
        Self::from_json(&json)
    }

    fn from_json(doc: &Value) -> Result<Self, SpecError> {
        let mut operations = Vec::new();
        for (path, item) in doc
            .get("paths")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            for method in METHODS {
                let Some(operation) = item.get(method) else {
                    continue;
                };
                let id = operation
                    .get("operationId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SpecError::Incomplete(format!("{method} {path} has no operationId"))
                    })?;
                operations.push(Operation {
                    id: id.to_owned(),
                    method: method.to_owned(),
                    path: path.clone(),
                });
            }
        }
        if operations.is_empty() {
            return Err(SpecError::Incomplete(
                "no operations under `paths`".to_owned(),
            ));
        }
        operations.sort();
        let ids: BTreeSet<&str> = operations.iter().map(|o| o.id.as_str()).collect();
        if ids.len() != operations.len() {
            return Err(SpecError::Incomplete("duplicate operationId".to_owned()));
        }
        let error_codes: BTreeSet<String> = doc
            .pointer("/components/schemas/ErrorCode/enum")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|code| code.as_str().map(str::to_owned))
            .collect();
        if error_codes.is_empty() {
            return Err(SpecError::Incomplete(
                "no `components.schemas.ErrorCode.enum`".to_owned(),
            ));
        }
        Ok(Self {
            operations,
            error_codes,
        })
    }

    /// Every operation, ordered by ID.
    #[must_use]
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    /// The operation with this ID.
    #[must_use]
    pub fn operation(&self, id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.id == id)
    }

    /// Whether `code` is in the `ErrorCode` enum.
    #[must_use]
    pub fn has_error_code(&self, code: &str) -> bool {
        self.error_codes.contains(code)
    }

    /// Every error code.
    #[must_use]
    pub const fn error_codes(&self) -> &BTreeSet<String> {
        &self.error_codes
    }
}

fn to_json(node: &Yaml<'_>) -> Result<Value, String> {
    match node {
        Yaml::Value(scalar) => Ok(match scalar {
            Scalar::Null => Value::Null,
            Scalar::Boolean(flag) => Value::Bool(*flag),
            Scalar::Integer(number) => Value::from(*number),
            Scalar::FloatingPoint(number) => Value::from(number.into_inner()),
            Scalar::String(text) => Value::String(text.to_string()),
        }),
        Yaml::Sequence(items) => items
            .iter()
            .map(to_json)
            .collect::<Result<_, _>>()
            .map(Value::Array),
        Yaml::Mapping(entries) => {
            let mut object = serde_json::Map::new();
            for (key, value) in entries {
                let Yaml::Value(Scalar::String(key)) = key else {
                    return Err(format!("unsupported mapping key {key:?}"));
                };
                if object.insert(key.to_string(), to_json(value)?).is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = "paths:\n  /a:\n    get:\n      operationId: getA\n  /b:\n    put:\n      operationId: putB\ncomponents:\n  schemas:\n    ErrorCode:\n      enum: [busy, not_found]\n";

    #[test]
    fn parses_operations_and_codes() {
        let spec = Spec::parse(MINI).unwrap();
        let ids: Vec<&str> = spec.operations().iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["getA", "putB"], "rule: operations sorted by ID");
        assert_eq!(spec.operation("putB").unwrap().method, "put");
        assert!(spec.has_error_code("busy") && !spec.has_error_code("nope"));
    }

    #[test]
    fn rejects_incomplete_documents() {
        for (text, what) in [
            ("paths:\n  /a:\n    get: {}\n", "operation without an ID"),
            ("paths: {}\n", "no operations"),
            (
                "paths:\n  /a:\n    get:\n      operationId: x\n",
                "no ErrorCode enum",
            ),
            ("a: 1\n---\nb: 2\n", "two documents"),
            ("a: 1\na: 2\n", "duplicate key"),
        ] {
            assert!(
                Spec::parse(text).is_err(),
                "rule: {what} is rejected; text={text:?}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_operation_ids() {
        let text = MINI.replace("putB", "getA");
        assert_eq!(
            Spec::parse(&text),
            Err(SpecError::Incomplete("duplicate operationId".to_owned())),
            "rule: operation IDs are unique"
        );
    }

    #[test]
    fn reads_the_real_spec() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../api/openapi.yaml");
        let spec = Spec::load(&path).unwrap();
        assert!(
            spec.operation("putKey").is_some(),
            "operations={:?}",
            spec.operations()
        );
        assert!(spec.has_error_code("not_leader"));
        assert_eq!(
            spec.error_codes().len(),
            12,
            "codes={:?}",
            spec.error_codes()
        );
    }
}
