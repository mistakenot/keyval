//! Keys and values: [`Key`] (DM-1, DM-4) and [`Value`] (DM-2), parsed once at the
//! boundary (CD-3). The core never re-checks their limits.

use core::fmt;

/// A key (DM-1): 1 to [`Key::MAX_BYTES`] bytes of UTF-8, measured after decoding.
///
/// Invariants:
/// - its length in UTF-8 bytes is 1 to 256 (not characters, not escaped length);
/// - it is stored exactly as given: no case folding and no Unicode normalisation, so
///   composed and decomposed forms are different keys;
/// - keys order by the byte-wise lexicographic order of their UTF-8 encoding (DM-4),
///   which is `str`'s own order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key(String);

/// Why a string is not a valid [`Key`]. The HTTP boundary answers both with
/// `400 invalid_request` (PLAN.md §8.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The key is empty (DM-1; an empty SCAN bound is invalid too, §8.5).
    Empty,
    /// The key is longer than [`Key::MAX_BYTES`].
    TooLong {
        /// The rejected key's length in UTF-8 bytes.
        bytes: usize,
    },
}

impl Key {
    /// The longest key, in UTF-8 bytes (DM-1).
    pub const MAX_BYTES: usize = 256;

    /// Parses a decoded key.
    ///
    /// # Errors
    ///
    /// [`KeyError`] if `raw` is empty or longer than [`Key::MAX_BYTES`] bytes.
    pub fn parse(raw: String) -> Result<Self, KeyError> {
        if raw.is_empty() {
            Err(KeyError::Empty)
        } else if raw.len() > Self::MAX_BYTES {
            Err(KeyError::TooLong { bytes: raw.len() })
        } else {
            Ok(Self(raw))
        }
    }

    /// The key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key's length in UTF-8 bytes: 1 to [`Key::MAX_BYTES`].
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.0.len()
    }

    /// The key as an owned string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the key is empty"),
            Self::TooLong { bytes } => write!(
                f,
                "the key is {bytes} bytes; at most {} are allowed (DM-1)",
                Key::MAX_BYTES
            ),
        }
    }
}

impl std::error::Error for KeyError {}

/// A value (DM-2): 0 to [`Value::MAX_BYTES`] bytes of UTF-8, measured after decoding.
///
/// Invariant: its length in UTF-8 bytes is at most 65,536 (64 KiB); it is stored exactly
/// as given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value(String);

/// Why a string is not a valid [`Value`]. The HTTP boundary answers it with
/// `413 too_large` (PLAN.md §8.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueError {
    /// The value is longer than [`Value::MAX_BYTES`].
    TooLong {
        /// The rejected value's length in UTF-8 bytes.
        bytes: usize,
    },
}

impl Value {
    /// The longest value, in UTF-8 bytes (DM-2): 64 KiB.
    pub const MAX_BYTES: usize = 65_536;

    /// The empty value, which is valid (DM-2).
    pub const EMPTY: Self = Self(String::new());

    /// Parses a decoded value.
    ///
    /// # Errors
    ///
    /// [`ValueError`] if `raw` is longer than [`Value::MAX_BYTES`] bytes.
    pub fn parse(raw: String) -> Result<Self, ValueError> {
        if raw.len() > Self::MAX_BYTES {
            Err(ValueError::TooLong { bytes: raw.len() })
        } else {
            Ok(Self(raw))
        }
    }

    /// The value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The value's length in UTF-8 bytes: 0 to [`Value::MAX_BYTES`].
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.0.len()
    }

    /// The value as an owned string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { bytes } => write!(
                f,
                "the value is {bytes} bytes; at most {} are allowed (DM-2)",
                Value::MAX_BYTES
            ),
        }
    }
}

impl std::error::Error for ValueError {}

#[cfg(test)]
mod tests {
    // check: KEY-VALUE

    use super::*;

    /// `unit` repeated `times` times, with its length in bytes for the assertion message.
    fn text(unit: &str, times: usize) -> (String, usize) {
        let text = unit.repeat(times);
        let bytes = text.len();
        (text, bytes)
    }

    #[test]
    fn keys_accept_1_to_256_bytes_counted_after_decoding() {
        let cases = [
            ("a", 0, Err(KeyError::Empty)),
            ("a", 1, Ok(())),
            ("a", 255, Ok(())),
            ("a", 256, Ok(())),
            ("a", 257, Err(KeyError::TooLong { bytes: 257 })),
            ("é", 128, Ok(())), // 256 bytes, 128 characters
            ("é", 129, Err(KeyError::TooLong { bytes: 258 })), // 258 bytes, 129 characters
            ("€", 85, Ok(())),  // 255 bytes, 85 characters
            ("€", 86, Err(KeyError::TooLong { bytes: 258 })),
            ("😀", 64, Ok(())), // 256 bytes, 64 characters
            ("😀", 65, Err(KeyError::TooLong { bytes: 260 })),
        ];
        for (unit, times, expected) in cases {
            let (raw, bytes) = text(unit, times);
            let parsed = Key::parse(raw.clone());
            assert_eq!(
                parsed.as_ref().map(|_| ()).map_err(|err| *err),
                expected,
                "rule: a key is 1..=256 decoded UTF-8 bytes (DM-1); unit={unit:?} times={times} bytes={bytes}"
            );
            if let Ok(key) = parsed {
                assert_eq!(
                    key.as_str(),
                    raw,
                    "rule: a key is stored exactly; bytes={bytes}"
                );
                assert_eq!(
                    key.byte_len(),
                    bytes,
                    "rule: byte_len counts UTF-8 bytes; unit={unit:?}"
                );
            }
        }
    }

    #[test]
    fn values_accept_0_to_65536_bytes_counted_after_decoding() {
        let cases = [
            ("v", 0, Ok(())),
            ("v", 1, Ok(())),
            ("v", 65_535, Ok(())),
            ("v", 65_536, Ok(())),
            ("v", 65_537, Err(ValueError::TooLong { bytes: 65_537 })),
            ("€", 21_845, Ok(())), // 65,535 bytes
            ("€", 21_846, Err(ValueError::TooLong { bytes: 65_538 })),
            ("😀", 16_384, Ok(())), // 65,536 bytes
            ("😀", 16_385, Err(ValueError::TooLong { bytes: 65_540 })), // 16,385 characters
        ];
        for (unit, times, expected) in cases {
            let (raw, bytes) = text(unit, times);
            let parsed = Value::parse(raw.clone());
            assert_eq!(
                parsed.as_ref().map(|_| ()).map_err(|err| *err),
                expected,
                "rule: a value is 0..=65,536 decoded UTF-8 bytes (DM-2); unit={unit:?} times={times} bytes={bytes}"
            );
            if let Ok(value) = parsed {
                assert_eq!(
                    value.as_str(),
                    raw,
                    "rule: a value is stored exactly; bytes={bytes}"
                );
                assert_eq!(
                    value.byte_len(),
                    bytes,
                    "rule: byte_len counts UTF-8 bytes; unit={unit:?}"
                );
            }
        }
        assert_eq!(
            Value::EMPTY.byte_len(),
            0,
            "rule: the empty value is valid (DM-2)"
        );
        assert_eq!(
            Value::parse(String::new()),
            Ok(Value::EMPTY),
            "rule: parsing \"\" gives the empty value"
        );
    }

    #[test]
    fn composed_and_decomposed_forms_are_different_keys() {
        let composed = Key::parse("caf\u{e9}".to_owned()).unwrap(); // é as U+00E9
        let decomposed = Key::parse("cafe\u{301}".to_owned()).unwrap(); // e + U+0301
        assert_ne!(
            composed, decomposed,
            "rule: no Unicode normalisation (DM-1)"
        );
        assert_eq!(
            (composed.byte_len(), decomposed.byte_len()),
            (5, 6),
            "rule: each form keeps its own bytes; composed={composed:?} decomposed={decomposed:?}"
        );
        let upper = Key::parse("CAFE".to_owned()).unwrap();
        let lower = Key::parse("cafe".to_owned()).unwrap();
        assert_ne!(upper, lower, "rule: no case folding (DM-1)");
    }

    #[test]
    fn keys_order_by_their_utf8_bytes() {
        let raw = [
            "b", "a", "Z", "aa", "a\u{0}", "\u{7f}", "\u{80}", "é", "e\u{301}", "€", "\u{ffff}",
            "😀", "~",
        ];
        let mut by_key: Vec<Key> = raw
            .iter()
            .map(|text| Key::parse((*text).to_owned()).unwrap())
            .collect();
        by_key.sort();
        let mut by_bytes: Vec<&[u8]> = raw.iter().map(|text| text.as_bytes()).collect();
        by_bytes.sort_unstable(); // computed independently of Key's Ord
        let keys: Vec<&[u8]> = by_key.iter().map(|key| key.as_str().as_bytes()).collect();
        assert_eq!(
            keys, by_bytes,
            "rule: keys order byte-wise by their UTF-8 encoding (DM-4)"
        );
        // UTF-16 code-unit order (as in JavaScript) would put U+1F600 before U+FFFF; DM-4 does not.
        let bmp = Key::parse("\u{ffff}".to_owned()).unwrap();
        let astral = Key::parse("😀".to_owned()).unwrap();
        assert!(
            bmp < astral,
            "rule: EF BF BF < F0 9F 98 80 byte-wise (DM-4); bmp={bmp:?} astral={astral:?}"
        );
    }

    #[test]
    fn errors_say_what_was_wrong() {
        assert_eq!(
            KeyError::Empty.to_string(),
            "the key is empty",
            "rule: errors are diagnosable (CD-6)"
        );
        assert_eq!(
            KeyError::TooLong { bytes: 257 }.to_string(),
            "the key is 257 bytes; at most 256 are allowed (DM-1)",
            "rule: the error names the length and the limit"
        );
        assert_eq!(
            ValueError::TooLong { bytes: 65_537 }.to_string(),
            "the value is 65537 bytes; at most 65536 are allowed (DM-2)",
            "rule: the error names the length and the limit"
        );
    }
}
