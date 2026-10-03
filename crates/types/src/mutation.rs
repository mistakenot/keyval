//! Writes: [`ExpectedVersion`] and [`Mutation`] (PLAN.md F-1, F-3, F-5, §7.5, CD-2).

use crate::data::{Key, Value};
use crate::log_index::Version;

/// What a compare-and-swap expects the key's state to be when its entry is applied
/// (F-5, D-35, C-5).
///
/// Invariant: exactly one of the two forms; there is no "any version" expectation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpectedVersion {
    /// The key must be absent.
    Absent,
    /// The key must be present with exactly this version.
    Version(Version),
}

impl ExpectedVersion {
    /// Whether a key whose current version is `current` (`None` when absent) meets the
    /// expectation.
    #[must_use]
    pub fn matches(self, current: Option<Version>) -> bool {
        match self {
            Self::Absent => current.is_none(),
            Self::Version(expected) => current == Some(expected),
        }
    }
}

/// A write command (PLAN.md §7.5 "Commands"), as parsed at the boundary (CD-3).
///
/// Invariant: every field is already valid: keys and values within DM-1/DM-2, and a
/// CAS carries exactly one expectation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mutation {
    /// Set the key to the value (F-1).
    Put {
        /// The key to set.
        key: Key,
        /// Its new value.
        value: Value,
    },
    /// Remove the key; succeeds whether or not it existed (F-3).
    Delete {
        /// The key to remove.
        key: Key,
    },
    /// Set the key to the value only if `expected` holds when the entry is applied (F-5).
    Cas {
        /// The key to set.
        key: Key,
        /// The state the key must be in.
        expected: ExpectedVersion,
        /// Its new value, if the expectation holds.
        value: Value,
    },
}

impl Mutation {
    /// The key the mutation writes.
    #[must_use]
    pub const fn key(&self) -> &Key {
        match self {
            Self::Put { key, .. } | Self::Delete { key } | Self::Cas { key, .. } => key,
        }
    }

    /// The value the mutation would store, or `None` for a delete.
    #[must_use]
    pub const fn value(&self) -> Option<&Value> {
        match self {
            Self::Put { value, .. } | Self::Cas { value, .. } => Some(value),
            Self::Delete { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::KeyValueLimits;

    fn key(text: &str) -> Key {
        Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
    }

    fn value(text: &str) -> Value {
        Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
    }

    #[test]
    fn expectations_match_exact_state_only() {
        let v41 = Version::new(41).unwrap();
        let v57 = Version::new(57).unwrap();
        let cases = [
            (ExpectedVersion::Absent, None, true),
            (ExpectedVersion::Absent, Some(v41), false),
            (ExpectedVersion::Version(v41), Some(v41), true),
            (ExpectedVersion::Version(v41), Some(v57), false),
            (ExpectedVersion::Version(v41), None, false),
        ];
        for (expected, current, matches) in cases {
            assert_eq!(
                expected.matches(current),
                matches,
                "rule: absent matches only absence, a version only itself (F-5); expected={expected:?} current={current:?}"
            );
        }
    }

    #[test]
    fn every_mutation_names_its_key_and_value() {
        let put = Mutation::Put {
            key: key("a"),
            value: value("1"),
        };
        let delete = Mutation::Delete { key: key("b") };
        let cas = Mutation::Cas {
            key: key("c"),
            expected: ExpectedVersion::Absent,
            value: Value::EMPTY,
        };
        assert_eq!(put.key().as_str(), "a", "rule: Put writes its key");
        assert_eq!(delete.key().as_str(), "b", "rule: Delete writes its key");
        assert_eq!(cas.key().as_str(), "c", "rule: Cas writes its key");
        assert_eq!(
            put.value().map(Value::as_str),
            Some("1"),
            "rule: Put stores its value"
        );
        assert_eq!(delete.value(), None, "rule: Delete stores no value (F-3)");
        assert_eq!(
            cas.value(),
            Some(&Value::EMPTY),
            "rule: Cas stores its value, empty allowed (DM-2)"
        );
    }
}
