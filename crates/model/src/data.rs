//! The model's own data types (PLAN.md §6.2, §8.1), defined here from the plan and never
//! borrowed from production code (T-12).

use std::error::Error;
use std::fmt;

/// The longest key: 256 bytes of UTF-8 (DM-1).
pub const MAX_KEY_BYTES: u64 = 256;

/// The longest value: 65,536 bytes (64 KiB) of UTF-8 (DM-2).
pub const MAX_VALUE_BYTES: u64 = 65_536;

/// The largest integer the API carries: 2^53 − 1 (§8.1). Indices and versions stay at or
/// below it.
pub const MAX_INTEGER: u64 = (1 << 53) - 1;

/// Why a value is not one of the model's data types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataError {
    /// A key with no bytes (DM-1: keys are 1 to 256 bytes).
    EmptyKey,
    /// A key longer than its limit, in bytes after decoding.
    KeyTooLong {
        /// The key's length.
        bytes: u64,
        /// The limit it exceeds.
        limit: u64,
    },
    /// A value longer than its limit, in bytes after decoding.
    ValueTooLong {
        /// The value's length.
        bytes: u64,
        /// The limit it exceeds.
        limit: u64,
    },
    /// A request ID that is not 32 lowercase hex characters (F-6).
    RequestId,
    /// An index or version outside 1 to 2^53 − 1.
    OutOfRange {
        /// The value given.
        value: u64,
    },
    /// A configured limit outside its allowed range.
    Limit {
        /// The limit's configuration name.
        name: &'static str,
        /// The value given.
        value: u64,
    },
}

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyKey => f.write_str("a key needs at least one byte"),
            Self::KeyTooLong { bytes, limit } => {
                write!(f, "a {bytes}-byte key exceeds the {limit}-byte limit")
            }
            Self::ValueTooLong { bytes, limit } => {
                write!(f, "a {bytes}-byte value exceeds the {limit}-byte limit")
            }
            Self::RequestId => f.write_str("a request ID is 32 lowercase hex characters"),
            Self::OutOfRange { value } => write!(f, "{value} is outside 1 to 2^53 - 1"),
            Self::Limit { name, value } => write!(f, "{name} = {value} is out of range"),
        }
    }
}

impl Error for DataError {}

/// The length of `text` in bytes.
fn bytes_of(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

/// A key: 1 to 256 bytes of UTF-8, stored exactly, with no normalisation (DM-1).
///
/// Keys order byte-wise by their UTF-8 encoding (DM-4), which is how `String` orders.
/// Invariant: 1 to [`MAX_KEY_BYTES`] bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key(String);

impl Key {
    /// The key `text`.
    ///
    /// # Errors
    ///
    /// [`DataError::EmptyKey`] or [`DataError::KeyTooLong`] past 256 bytes.
    pub fn new(text: impl Into<String>) -> Result<Self, DataError> {
        let text = text.into();
        let bytes = bytes_of(&text);
        if bytes == 0 {
            Err(DataError::EmptyKey)
        } else if bytes > MAX_KEY_BYTES {
            Err(DataError::KeyTooLong {
                bytes,
                limit: MAX_KEY_BYTES,
            })
        } else {
            Ok(Self(text))
        }
    }

    /// The key's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key's length in bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        bytes_of(&self.0)
    }
}

/// A value: 0 to 65,536 bytes of UTF-8 (DM-2).
///
/// Invariant: at most [`MAX_VALUE_BYTES`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value(String);

impl Value {
    /// The value `text`.
    ///
    /// # Errors
    ///
    /// [`DataError::ValueTooLong`] past 64 KiB.
    pub fn new(text: impl Into<String>) -> Result<Self, DataError> {
        let text = text.into();
        let bytes = bytes_of(&text);
        if bytes > MAX_VALUE_BYTES {
            Err(DataError::ValueTooLong {
                bytes,
                limit: MAX_VALUE_BYTES,
            })
        } else {
            Ok(Self(text))
        }
    }

    /// The value's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The value's length in bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        bytes_of(&self.0)
    }
}

/// A write's request ID: 128 bits, written as 32 lowercase hex characters (F-6).
///
/// Invariant: none beyond its 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestId([u8; 16]);

impl RequestId {
    /// The request ID with these 16 bytes, most significant first.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Parses an `Idempotency-Key`: exactly 32 lowercase hex characters.
    ///
    /// # Errors
    ///
    /// [`DataError::RequestId`] for anything else, uppercase hex included.
    pub fn parse(text: &str) -> Result<Self, DataError> {
        let digits = text.as_bytes();
        if digits.len() != 32 {
            return Err(DataError::RequestId);
        }
        let nibble = |digit: u8| match digit {
            b'0'..=b'9' => Ok(digit - b'0'),
            b'a'..=b'f' => Ok(digit - b'a' + 10),
            _ => Err(DataError::RequestId),
        };
        let mut bytes = [0; 16];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            let [high, low] = pair else {
                return Err(DataError::RequestId);
            };
            *byte = (nibble(*high)? << 4) | nibble(*low)?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// A log index at which an entry is applied: 1 to 2^53 − 1 (§8.1).
///
/// Invariant: 1 to [`MAX_INTEGER`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Index(u64);

impl Index {
    /// The index `value`.
    ///
    /// # Errors
    ///
    /// [`DataError::OutOfRange`] outside 1 to 2^53 − 1.
    pub const fn new(value: u64) -> Result<Self, DataError> {
        if value == 0 || value > MAX_INTEGER {
            Err(DataError::OutOfRange { value })
        } else {
            Ok(Self(value))
        }
    }

    /// The raw index.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A key's version: the log index of the write that set it (DM-3).
///
/// Invariant: 1 to [`MAX_INTEGER`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64);

impl Version {
    /// The version `value`, as a CAS expectation names it.
    ///
    /// # Errors
    ///
    /// [`DataError::OutOfRange`] outside 1 to 2^53 − 1 (a CAS `expected.version` at or
    /// above 2^53 is invalid, §8.1).
    pub const fn new(value: u64) -> Result<Self, DataError> {
        if value == 0 || value > MAX_INTEGER {
            Err(DataError::OutOfRange { value })
        } else {
            Ok(Self(value))
        }
    }

    /// The version a write executed at `index` gives its key: the same number (DM-3).
    #[must_use]
    pub const fn at(index: Index) -> Self {
        Self(index.0)
    }

    /// The raw version.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The configured limits the model applies (§6.2, §7.10): the key and value limits in
/// force (D-80, at most the DM-1/DM-2 maxima) and the live-state capacity (DM-6).
///
/// Invariant: key limit 1 to 256, value limit 0 to 65,536, live limits at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    key_bytes: u64,
    value_bytes: u64,
    live_keys: u64,
    live_bytes: u64,
}

impl Limits {
    /// The plan's defaults: 256-byte keys, 64 KiB values, 10,000 live keys and 64 MiB of
    /// live key and value bytes (§7.10, DM-6).
    pub const DEFAULT: Self = Self {
        key_bytes: MAX_KEY_BYTES,
        value_bytes: MAX_VALUE_BYTES,
        live_keys: 10_000,
        live_bytes: 64 << 20,
    };

    /// Limits as configured.
    ///
    /// # Errors
    ///
    /// [`DataError::Limit`] for a key limit outside 1 to 256, a value limit above 65,536,
    /// or a live limit of 0.
    pub const fn new(
        max_key_bytes: u64,
        max_value_bytes: u64,
        max_live_keys: u64,
        max_live_bytes: u64,
    ) -> Result<Self, DataError> {
        if max_key_bytes == 0 || max_key_bytes > MAX_KEY_BYTES {
            Err(DataError::Limit {
                name: "max_key_bytes",
                value: max_key_bytes,
            })
        } else if max_value_bytes > MAX_VALUE_BYTES {
            Err(DataError::Limit {
                name: "max_value_bytes",
                value: max_value_bytes,
            })
        } else if max_live_keys == 0 {
            Err(DataError::Limit {
                name: "max_live_keys",
                value: max_live_keys,
            })
        } else if max_live_bytes == 0 {
            Err(DataError::Limit {
                name: "max_live_bytes",
                value: max_live_bytes,
            })
        } else {
            Ok(Self {
                key_bytes: max_key_bytes,
                value_bytes: max_value_bytes,
                live_keys: max_live_keys,
                live_bytes: max_live_bytes,
            })
        }
    }

    /// The key limit in force, in bytes.
    #[must_use]
    pub const fn max_key_bytes(self) -> u64 {
        self.key_bytes
    }

    /// The value limit in force, in bytes.
    #[must_use]
    pub const fn max_value_bytes(self) -> u64 {
        self.value_bytes
    }

    /// The most live keys (DM-6).
    #[must_use]
    pub const fn max_live_keys(self) -> u64 {
        self.live_keys
    }

    /// The most live key and value bytes (DM-6).
    #[must_use]
    pub const fn max_live_bytes(self) -> u64 {
        self.live_bytes
    }

    /// Checks `key` against the key limit in force.
    ///
    /// # Errors
    ///
    /// [`DataError::KeyTooLong`].
    pub fn check_key(self, key: &Key) -> Result<(), DataError> {
        if key.bytes() > self.key_bytes {
            Err(DataError::KeyTooLong {
                bytes: key.bytes(),
                limit: self.key_bytes,
            })
        } else {
            Ok(())
        }
    }

    /// Checks `value` against the value limit in force.
    ///
    /// # Errors
    ///
    /// [`DataError::ValueTooLong`].
    pub fn check_value(self, value: &Value) -> Result<(), DataError> {
        if value.bytes() > self.value_bytes {
            Err(DataError::ValueTooLong {
                bytes: value.bytes(),
                limit: self.value_bytes,
            })
        } else {
            Ok(())
        }
    }
}

/// The history capacity a leader admits new writes under (DM-7, §7.10): once its commit
/// index reaches `max_log_entries`, it refuses them before proposal with `history_full`.
///
/// DM-7's byte ceiling, `max_history_bytes`, is not here yet: it counts each command's
/// canonical encoded length, which the plan does not define independently of the codec
/// (br-camk).
///
/// Invariant: `max_log_entries` is 1 to 2^53 − 1, so every index it admits is one the API
/// carries (§8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryLimits {
    log_entries: u64,
}

impl HistoryLimits {
    /// The plan's default: 1,000,000 log entries (§7.10, DM-7).
    pub const DEFAULT: Self = Self {
        log_entries: 1_000_000,
    };

    /// Limits as configured.
    ///
    /// # Errors
    ///
    /// [`DataError::Limit`] for a `max_log_entries` of 0 or above 2^53 − 1.
    pub const fn new(max_log_entries: u64) -> Result<Self, DataError> {
        if max_log_entries == 0 || max_log_entries > MAX_INTEGER {
            Err(DataError::Limit {
                name: "max_log_entries",
                value: max_log_entries,
            })
        } else {
            Ok(Self {
                log_entries: max_log_entries,
            })
        }
    }

    /// The commit-index ceiling (DM-7).
    #[must_use]
    pub const fn max_log_entries(self) -> u64 {
        self.log_entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_1_to_256_bytes_measured_after_decoding() {
        for (text, ok) in [
            (String::new(), false),
            ("k".to_owned(), true),
            ("k".repeat(255), true),
            ("k".repeat(256), true),
            ("k".repeat(257), false),
            // 128 two-byte characters are 256 bytes: the limit counts bytes, not characters.
            ("\u{e9}".repeat(128), true),
            (format!("{}\u{e9}", "k".repeat(255)), false),
        ] {
            let bytes = text.len();
            assert_eq!(
                Key::new(text).is_ok(),
                ok,
                "rule: a key is 1 to 256 bytes of UTF-8 (DM-1); bytes={bytes}"
            );
        }
        assert_eq!(Key::new(""), Err(DataError::EmptyKey));
    }

    #[test]
    fn keys_are_stored_exactly_and_ordered_by_their_bytes() {
        let composed = Key::new("\u{e9}").unwrap();
        let decomposed = Key::new("e\u{301}").unwrap();
        assert_ne!(
            composed, decomposed,
            "rule: no Unicode normalisation, so composed and decomposed forms are different keys \
             (DM-1)"
        );
        let mut keys: Vec<Key> = ["b", "\u{e9}", "a", "B", "a\u{0}", "e\u{301}"]
            .into_iter()
            .map(|text| Key::new(text).unwrap())
            .collect();
        keys.sort();
        let order: Vec<&str> = keys.iter().map(Key::as_str).collect();
        assert_eq!(
            order,
            ["B", "a", "a\u{0}", "b", "e\u{301}", "\u{e9}"],
            "rule: keys order byte-wise by their UTF-8 encoding (DM-4)"
        );
    }

    #[test]
    fn values_are_0_to_65536_bytes() {
        assert!(
            Value::new("").is_ok(),
            "rule: an empty value is valid (DM-2)"
        );
        assert!(Value::new("v".repeat(65_536)).is_ok());
        assert_eq!(
            Value::new("v".repeat(65_537)),
            Err(DataError::ValueTooLong {
                bytes: 65_537,
                limit: 65_536
            }),
            "rule: a value is at most 64 KiB (DM-2)"
        );
    }

    #[test]
    fn request_ids_are_32_lowercase_hex_characters() {
        let text = "0123456789abcdef0123456789abcdef";
        let id = RequestId::parse(text).unwrap();
        assert_eq!(id.to_string(), text, "rule: it prints as it was given");
        for bad in [
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789abcdef0123456789abcdeg",
            "",
        ] {
            assert_eq!(
                RequestId::parse(bad),
                Err(DataError::RequestId),
                "rule: exactly 32 lowercase hex characters (F-6); bad={bad:?}"
            );
        }
    }

    #[test]
    fn indices_and_versions_are_1_to_2_pow_53_minus_1() {
        for (value, ok) in [
            (0, false),
            (1, true),
            (MAX_INTEGER, true),
            (MAX_INTEGER + 1, false),
        ] {
            assert_eq!(Index::new(value).is_ok(), ok, "rule: §8.1; value={value}");
            assert_eq!(Version::new(value).is_ok(), ok, "rule: §8.1; value={value}");
        }
        assert_eq!(
            Version::at(Index::new(7).unwrap()),
            Version::new(7).unwrap(),
            "rule: a write at index i gives version i (DM-3)"
        );
    }

    #[test]
    fn limits_default_to_the_plan_and_bound_configured_keys_and_values() {
        let defaults = Limits::DEFAULT;
        assert_eq!(
            (
                defaults.max_key_bytes(),
                defaults.max_value_bytes(),
                defaults.max_live_keys(),
                defaults.max_live_bytes()
            ),
            (256, 65_536, 10_000, 67_108_864),
            "rule: the plan's defaults (§7.10, DM-6)"
        );
        let small = Limits::new(16, 64, 8, 1024).unwrap();
        assert_eq!(small.check_key(&Key::new("k".repeat(16)).unwrap()), Ok(()));
        assert_eq!(
            small.check_key(&Key::new("k".repeat(17)).unwrap()),
            Err(DataError::KeyTooLong {
                bytes: 17,
                limit: 16
            }),
            "rule: the configured key limit is in force (D-80)"
        );
        assert_eq!(
            small.check_value(&Value::new("v".repeat(65)).unwrap()),
            Err(DataError::ValueTooLong {
                bytes: 65,
                limit: 64
            }),
            "rule: the configured value limit is in force (D-80)"
        );
        assert!(
            Limits::new(257, 64, 8, 1024).is_err(),
            "rule: at most DM-1's maximum"
        );
        assert!(
            Limits::new(16, 65_537, 8, 1024).is_err(),
            "rule: at most DM-2's maximum"
        );
        assert!(Limits::new(0, 64, 8, 1024).is_err());
        assert!(Limits::new(16, 64, 0, 1024).is_err());
    }

    #[test]
    fn history_limits_default_to_the_plan_and_are_1_to_2_pow_53_minus_1() {
        assert_eq!(
            HistoryLimits::DEFAULT.max_log_entries(),
            1_000_000,
            "rule: the plan's default (§7.10, DM-7)"
        );
        assert_eq!(
            HistoryLimits::new(1).map(HistoryLimits::max_log_entries),
            Ok(1),
            "rule: a one-entry ceiling is the smallest"
        );
        assert_eq!(
            HistoryLimits::new(0),
            Err(DataError::Limit {
                name: "max_log_entries",
                value: 0
            }),
            "rule: a zero ceiling would refuse every write"
        );
        assert_eq!(
            HistoryLimits::new(MAX_INTEGER).map(HistoryLimits::max_log_entries),
            Ok(MAX_INTEGER),
            "rule: the largest ceiling is the largest index the API carries (§8.1)"
        );
        assert_eq!(
            HistoryLimits::new(MAX_INTEGER + 1),
            Err(DataError::Limit {
                name: "max_log_entries",
                value: MAX_INTEGER + 1
            }),
            "rule: a ceiling above 2^53 - 1 would admit an index the API cannot carry (§8.1)"
        );
    }
}
