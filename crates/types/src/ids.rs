//! Identifiers: [`NodeId`], [`ClusterId`] and [`RequestId`] (CD-4).

use core::fmt;

use crate::range::{OutOfRange, Quantity};

/// A configured node's identity (PLAN.md §7.10).
///
/// Invariant: nonzero and at most [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER)
/// (D-77), so it is a safe JSON integer wherever it is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeId(u64);

impl NodeId {
    /// Parses a raw node ID.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] if `raw` is zero or above [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER).
    pub const fn new(raw: u64) -> Result<Self, OutOfRange> {
        match Quantity::NodeId.check(raw) {
            Ok(value) => Ok(Self(value)),
            Err(err) => Err(err),
        }
    }

    /// The raw ID.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The cluster's configured display name, such as `keyval-local-v1` (PLAN.md §7.10).
/// It is a label only: the random cluster instance ID, not this name, keeps two clusters
/// apart (§7.7 step 0, D-63).
///
/// Invariant: 1 to [`ClusterId::MAX_BYTES`] bytes of UTF-8, stored exactly.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClusterId(String);

/// Why a string is not a valid [`ClusterId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClusterIdError {
    /// The name is empty.
    Empty,
    /// The name is longer than [`ClusterId::MAX_BYTES`]; `bytes` is its length.
    TooLong {
        /// The rejected name's length in UTF-8 bytes.
        bytes: usize,
    },
}

impl ClusterId {
    /// The longest name, in UTF-8 bytes. It bounds the genesis frame's encoding (CD-9).
    pub const MAX_BYTES: usize = 255;

    /// Parses a cluster display name.
    ///
    /// # Errors
    ///
    /// [`ClusterIdError`] if `raw` is empty or longer than [`ClusterId::MAX_BYTES`] bytes.
    pub fn parse(raw: String) -> Result<Self, ClusterIdError> {
        if raw.is_empty() {
            Err(ClusterIdError::Empty)
        } else if raw.len() > Self::MAX_BYTES {
            Err(ClusterIdError::TooLong { bytes: raw.len() })
        } else {
            Ok(Self(raw))
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for ClusterIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the cluster ID is empty"),
            Self::TooLong { bytes } => write!(
                f,
                "the cluster ID is {bytes} bytes; at most {} are allowed",
                ClusterId::MAX_BYTES
            ),
        }
    }
}

impl std::error::Error for ClusterIdError {}

/// A write's request ID: 128 bits, written as exactly 32 lowercase hexadecimal characters
/// (F-6). It names one logical write for the lifetime of the cluster (DM-5).
///
/// Invariant: none beyond its 16 bytes; every 128-bit value is a valid ID, and its text
/// form is canonical (lowercase, no separators).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestId([u8; 16]);

/// Why a string is not a valid [`RequestId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestIdError {
    /// The text is not exactly 32 bytes long; `bytes` is its length.
    WrongLength {
        /// The rejected text's length in bytes.
        bytes: usize,
    },
    /// The byte at `at` is not one of `0-9` or `a-f`.
    NotLowercaseHex {
        /// The offending byte's position, from 0.
        at: usize,
    },
}

impl RequestId {
    /// The length of the text form.
    pub const TEXT_LEN: usize = 32;

    /// Parses the text form: exactly 32 characters from `0-9a-f`.
    ///
    /// # Errors
    ///
    /// [`RequestIdError`] on any other length, or on any character outside `0-9a-f`
    /// (uppercase included).
    pub fn parse(text: &str) -> Result<Self, RequestIdError> {
        let bytes = text.as_bytes();
        if bytes.len() != Self::TEXT_LEN {
            return Err(RequestIdError::WrongLength { bytes: bytes.len() });
        }
        let mut id = [0_u8; 16];
        for (index, &[high, low]) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let nibble = |at: usize, byte: u8| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err(RequestIdError::NotLowercaseHex { at }),
            };
            let value = (nibble(2 * index, high)? << 4) | nibble(2 * index + 1, low)?;
            if let Some(slot) = id.get_mut(index) {
                *slot = value;
            }
        }
        Ok(Self(id))
    }

    /// An ID from its 16 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The ID's 16 bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Display for RequestIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { bytes } => write!(
                f,
                "a request ID is {} characters; this one is {bytes} bytes",
                RequestId::TEXT_LEN
            ),
            Self::NotLowercaseHex { at } => {
                write!(f, "character {at} of the request ID is not 0-9 or a-f")
            }
        }
    }
}

impl std::error::Error for RequestIdError {}

#[cfg(test)]
mod tests {
    // check: NEWTYPES

    use super::*;
    use crate::MAX_SAFE_INTEGER;

    #[test]
    fn node_ids_are_nonzero_and_safe_json_integers() {
        for (raw, ok) in [
            (0, false),
            (1, true),
            (MAX_SAFE_INTEGER, true),
            (MAX_SAFE_INTEGER + 1, false),
            (u64::MAX, false),
        ] {
            let parsed = NodeId::new(raw);
            assert_eq!(
                parsed.is_ok(),
                ok,
                "rule: node IDs are 1..=2^53-1 (§7.10, D-77); raw={raw} parsed={parsed:?}"
            );
            if let Ok(id) = parsed {
                assert_eq!(id.get(), raw, "rule: a node ID keeps its value; raw={raw}");
            }
        }
        assert_eq!(
            NodeId::new(0),
            Err(OutOfRange {
                quantity: Quantity::NodeId,
                value: 0
            }),
            "rule: the error names the quantity and the value"
        );
    }

    #[test]
    fn cluster_ids_are_one_to_255_bytes_stored_exactly() {
        assert_eq!(
            ClusterId::parse(String::new()),
            Err(ClusterIdError::Empty),
            "rule: an empty name is refused"
        );
        let longest = "é".repeat(127) + "x"; // 255 bytes
        assert_eq!(
            longest.len(),
            255,
            "fixture: 127 two-byte characters plus one byte"
        );
        let parsed = ClusterId::parse(longest.clone()).map(|id| id.as_str().to_owned());
        assert_eq!(
            parsed,
            Ok(longest.clone()),
            "rule: 255 bytes is the longest name, stored exactly"
        );
        let too_long = longest + "x";
        assert_eq!(
            ClusterId::parse(too_long),
            Err(ClusterIdError::TooLong { bytes: 256 }),
            "rule: 256 bytes is refused, counted in bytes"
        );
    }

    #[test]
    fn request_ids_parse_exactly_32_lowercase_hex_characters() {
        let text = "d70ff69df7084a088e4a531837cc4e5a";
        let id = RequestId::parse(text).unwrap();
        assert_eq!(
            id.to_string(),
            text,
            "rule: the text form round-trips (F-6); text={text}"
        );
        assert_eq!(
            id.to_bytes()[0],
            0xd7,
            "rule: the first two characters are the first byte"
        );
        assert_eq!(
            RequestId::from_bytes(id.to_bytes()),
            id,
            "rule: bytes round-trip"
        );
        let cases = [
            (
                "D70ff69df7084a088e4a531837cc4e5a",
                RequestIdError::NotLowercaseHex { at: 0 },
            ),
            (
                "d70ff69df7084a088e4a531837cc4e5G",
                RequestIdError::NotLowercaseHex { at: 31 },
            ),
            (
                "d70ff69d-f708-4a08-8e4a-531837cc",
                RequestIdError::NotLowercaseHex { at: 8 },
            ),
            (
                "d70ff69df7084a088e4a531837cc4e5",
                RequestIdError::WrongLength { bytes: 31 },
            ),
            (
                "d70ff69df7084a088e4a531837cc4e5a0",
                RequestIdError::WrongLength { bytes: 33 },
            ),
            ("", RequestIdError::WrongLength { bytes: 0 }),
            (
                "é70ff69df7084a088e4a531837cc4e5",
                RequestIdError::NotLowercaseHex { at: 0 },
            ),
        ];
        for (text, error) in cases {
            assert_eq!(
                RequestId::parse(text),
                Err(error),
                "rule: only 32 chars of 0-9a-f (F-6); text={text:?}"
            );
        }
    }

    #[test]
    fn request_id_text_is_canonical_for_every_byte() {
        for byte in 0..=u8::MAX {
            let id = RequestId::from_bytes([byte; 16]);
            let text = id.to_string();
            assert_eq!(
                RequestId::parse(&text),
                Ok(id),
                "rule: display then parse is the identity; byte={byte:#04x}"
            );
            assert!(
                text.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "rule: the text form is lowercase hex; text={text}"
            );
        }
    }
}
