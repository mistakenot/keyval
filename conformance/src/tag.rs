//! Test tagging (PLAN.md §10.8, §10.13).
//!
//! Each conformance test declares its class and the operations and error codes it exercises. The registry gate reads the `// api-test:
//! <name>` marker beside each test; [`Tag::validate`] holds the declaration to the spec at
//! run time, so a typo cannot hide an uncovered operation.

use std::collections::BTreeSet;

use crate::spec::Spec;

/// A test's class (§10.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Behaviour a single node can show; runs on model, single node and cluster nodes.
    Semantic,
    /// Follower redirects, no-leader, health during recovery; runs on the cluster only.
    Topology,
}

impl Class {
    /// Registry spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Topology => "topology",
        }
    }
}

/// What one test claims to exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    /// The test's name.
    pub test: &'static str,
    /// Its class.
    pub class: Class,
    /// `operationId`s it exercises (at least one).
    pub operations: &'static [&'static str],
    /// Error codes it provokes (possibly none).
    pub error_codes: &'static [&'static str],
}

impl Tag {
    /// Every way this tag disagrees with the spec; empty when it is sound.
    #[must_use]
    pub fn validate(&self, spec: &Spec) -> Vec<String> {
        let mut problems = Vec::new();
        if self.operations.is_empty() {
            problems.push(format!("{}: exercises no operation", self.test));
        }
        for op in self.operations {
            if spec.operation(op).is_none() {
                problems.push(format!("{}: unknown operationId {op:?}", self.test));
            }
        }
        for code in self.error_codes {
            if !spec.has_error_code(code) {
                problems.push(format!("{}: unknown error code {code:?}", self.test));
            }
        }
        problems
    }
}

/// Validates a whole suite: every tag is sound and test names are unique.
#[must_use]
pub fn validate_all(tags: &[Tag], spec: &Spec) -> Vec<String> {
    let mut problems: Vec<String> = tags.iter().flat_map(|t| t.validate(spec)).collect();
    let mut seen = BTreeSet::new();
    for tag in tags {
        if !seen.insert(tag.test) {
            problems.push(format!("{}: tagged twice", tag.test));
        }
    }
    problems
}

/// Operations and error codes that no tag covers: what condition 6 of the registry gate
/// reports until W32.
#[must_use]
pub fn uncovered(tags: &[Tag], spec: &Spec) -> (Vec<String>, Vec<String>) {
    let ops: BTreeSet<&str> = tags
        .iter()
        .flat_map(|t| t.operations.iter().copied())
        .collect();
    let codes: BTreeSet<&str> = tags
        .iter()
        .flat_map(|t| t.error_codes.iter().copied())
        .collect();
    (
        spec.operations()
            .iter()
            .filter(|o| !ops.contains(o.id.as_str()))
            .map(|o| o.id.clone())
            .collect(),
        spec.error_codes()
            .iter()
            .filter(|c| !codes.contains(c.as_str()))
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../api/openapi.yaml");
        Spec::load(&path).unwrap()
    }

    const GOOD: Tag = Tag {
        test: "put_then_get",
        class: Class::Semantic,
        operations: &["putKey", "getKey"],
        error_codes: &["not_found"],
    };

    #[test]
    fn a_sound_tag_validates() {
        assert_eq!(GOOD.validate(&spec()), Vec::<String>::new());
    }

    #[test]
    fn typos_and_empty_tags_are_reported() {
        let bad = Tag {
            test: "t",
            class: Class::Topology,
            operations: &["putkey"],
            error_codes: &["nope"],
        };
        let problems = bad.validate(&spec());
        assert_eq!(problems.len(), 2, "problems={problems:?}");
        let empty = Tag {
            operations: &[],
            error_codes: &[],
            ..bad
        };
        assert_eq!(empty.validate(&spec()).len(), 1);
    }

    #[test]
    fn duplicate_names_and_coverage_gaps_are_found() {
        let spec = spec();
        let problems = validate_all(&[GOOD, GOOD], &spec);
        assert_eq!(problems, ["put_then_get: tagged twice"]);
        let (ops, codes) = uncovered(&[GOOD], &spec);
        assert!(
            !ops.contains(&"putKey".to_owned()) && ops.contains(&"scanKeys".to_owned()),
            "ops={ops:?}"
        );
        assert!(
            !codes.contains(&"not_found".to_owned()) && codes.contains(&"busy".to_owned()),
            "codes={codes:?}"
        );
    }
}
