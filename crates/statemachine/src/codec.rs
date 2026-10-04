//! The canonical command codec (PLAN.md §7.5 "Commands"; leaf area `statemachine/codec`).
//!
//! A log entry carries one command, encoded exactly one way, so equal commands have equal
//! bytes and equal digests (DM-5). Format version 1 ([`COMMAND_FORMAT_VERSION`]):
//!
//! | Field | Bytes | Encoding |
//! |---|---|---|
//! | format version | 1 | `1` |
//! | kind | 1 | `1` PUT, `2` DELETE, `3` compare-and-swap |
//! | request ID | 16 | its 128 bits, most significant byte first |
//! | key length | 4 | big-endian `u32` |
//! | key | key length | UTF-8: 1 byte to the key limit |
//! | expectation (compare-and-swap) | 1 | `0` absent, `1` a version |
//! | expected version (expectation `1`) | 8 | big-endian `u64`: 1 to 2^53 − 1 |
//! | value length (PUT, compare-and-swap) | 4 | big-endian `u32` |
//! | value (PUT, compare-and-swap) | value length | UTF-8: 0 bytes to the value limit |
//!
//! Every field is fixed-width or length-prefixed, with no padding, no alternatives and no
//! trailing bytes, so [`decode`] accepts only what [`encode`] produces:
//! `encode(decode(b)) == b` for every `b` it accepts. Keys and values are checked against
//! the configured limits in force (D-80).
//!
//! The envelope (every field but the key and value bytes) is at most
//! [`MAX_ENVELOPE_BYTES`], within [`COMMAND_ENVELOPE_BOUND`], so every valid command fits
//! [`MAX_COMMAND_BYTES`] (§7.10, §7.11); a test proves both. Decoding never panics, and
//! rejects an oversized input before reading it (§10.19).

use types::{
    ExpectedVersion, Key, KeyError, KeyValueLimits, MAX_COMMAND_BYTES, Mutation, OutOfRange,
    RequestId, Value, ValueError, Version, invariant,
};

#[cfg(doc)]
use types::{COMMAND_ENVELOPE_BOUND, COMMAND_FORMAT_VERSION};

/// The format version byte: [`COMMAND_FORMAT_VERSION`], which the semantic-config
/// fingerprint covers (DUR-8).
const FORMAT: u8 = 1;

/// The expectation byte of a compare-and-swap that expects the key to be absent.
const ABSENT: u8 = 0;

/// The expectation byte of a compare-and-swap that expects a version.
const AT_VERSION: u8 = 1;

/// The largest envelope, in bytes: every field of a compare-and-swap that expects a
/// version, except its key and value bytes (35 B).
pub const MAX_ENVELOPE_BYTES: usize = 1 + 1 + 16 + 4 + 1 + 8 + 4;

/// A write command as a log entry carries it (§7.5 "Commands").
///
/// Invariant: none beyond its parts', which are valid by construction (CD-3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The write's request ID (F-6).
    pub request_id: RequestId,
    /// What the write does.
    pub mutation: Mutation,
}

/// Which command an encoding holds: the kind byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Put,
    Delete,
    Cas,
}

impl Kind {
    const fn of(mutation: &Mutation) -> Self {
        match mutation {
            Mutation::Put { .. } => Self::Put,
            Mutation::Delete { .. } => Self::Delete,
            Mutation::Cas { .. } => Self::Cas,
        }
    }

    const fn byte(self) -> u8 {
        match self {
            Self::Put => 1,
            Self::Delete => 2,
            Self::Cas => 3,
        }
    }

    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Put),
            2 => Some(Self::Delete),
            3 => Some(Self::Cas),
            _ => None,
        }
    }
}

/// A field of the encoding, as named in the module's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// The format version byte.
    Format,
    /// The kind byte.
    Kind,
    /// The 16-byte request ID.
    RequestId,
    /// The key's length prefix.
    KeyLength,
    /// The key's bytes.
    Key,
    /// A compare-and-swap's expectation byte.
    Expectation,
    /// A compare-and-swap's expected version.
    ExpectedVersion,
    /// The value's length prefix.
    ValueLength,
    /// The value's bytes.
    Value,
}

/// Why bytes are not a canonical command (CD-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The input is longer than [`MAX_COMMAND_BYTES`]; nothing was read.
    TooLarge {
        /// The input's length.
        bytes: usize,
    },
    /// The input ended inside `field`.
    Truncated {
        /// The incomplete field.
        field: Field,
    },
    /// The format version is not this build's.
    UnknownFormat {
        /// The version byte found.
        version: u8,
    },
    /// The kind byte names no command.
    UnknownKind {
        /// The kind byte found.
        kind: u8,
    },
    /// A compare-and-swap's expectation byte is neither `0` nor `1`.
    UnknownExpectation {
        /// The expectation byte found.
        tag: u8,
    },
    /// A compare-and-swap's expected version is outside 1 to 2^53 − 1.
    ExpectedVersion(OutOfRange),
    /// The key or value bytes are not UTF-8.
    NotUtf8 {
        /// The key or the value.
        field: Field,
    },
    /// The key breaks the key limits in force.
    Key(KeyError),
    /// The value breaks the value limit in force.
    Value(ValueError),
    /// Bytes follow a complete command.
    TrailingBytes {
        /// How many.
        count: usize,
    },
}

/// The canonical bytes of `command` (format version 1).
#[must_use]
pub fn encode(command: &Command) -> Vec<u8> {
    let key = command.mutation.key().as_str().as_bytes();
    let value = command
        .mutation
        .value()
        .map(|value| value.as_str().as_bytes());
    let mut out = Vec::with_capacity(MAX_ENVELOPE_BYTES + key.len() + value.map_or(0, <[u8]>::len));
    out.push(FORMAT);
    out.push(Kind::of(&command.mutation).byte());
    out.extend_from_slice(&command.request_id.to_bytes());
    push_field(&mut out, key);
    match &command.mutation {
        Mutation::Cas {
            expected: ExpectedVersion::Absent,
            ..
        } => out.push(ABSENT),
        Mutation::Cas {
            expected: ExpectedVersion::Version(version),
            ..
        } => {
            out.push(AT_VERSION);
            out.extend_from_slice(&version.get().to_be_bytes());
        }
        Mutation::Put { .. } | Mutation::Delete { .. } => {}
    }
    if let Some(value) = value {
        push_field(&mut out, value);
    }
    out
}

/// Appends `bytes` after its big-endian `u32` length prefix.
fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len());
    invariant!(
        len.is_ok(),
        "SM-CODEC-FIELD-LEN",
        "a {}-byte key or value exceeds the command codec's u32 length prefix; DM-1 and \
         DM-2 bound both far below it",
        bytes.len()
    );
    out.extend_from_slice(&len.unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Decodes one canonical command, checking its key and value against `limits` (D-80).
///
/// # Errors
///
/// [`DecodeError`] for any input that [`encode`] would not produce for a command valid
/// under `limits`.
pub fn decode(bytes: &[u8], limits: KeyValueLimits) -> Result<Command, DecodeError> {
    if !u64::try_from(bytes.len()).is_ok_and(|len| len <= MAX_COMMAND_BYTES) {
        return Err(DecodeError::TooLarge { bytes: bytes.len() });
    }
    let mut input = Reader { rest: bytes };
    let format = input.byte(Field::Format)?;
    if format != FORMAT {
        return Err(DecodeError::UnknownFormat { version: format });
    }
    let kind = input.byte(Field::Kind)?;
    let kind = Kind::from_byte(kind).ok_or(DecodeError::UnknownKind { kind })?;
    let request_id = RequestId::from_bytes(input.array(Field::RequestId)?);
    let key = input.text(Field::KeyLength, Field::Key)?;
    let key = Key::parse(key, limits).map_err(DecodeError::Key)?;
    let mutation = match kind {
        Kind::Put => Mutation::Put {
            key,
            value: input.value(limits)?,
        },
        Kind::Delete => Mutation::Delete { key },
        Kind::Cas => {
            let expected = input.expectation()?;
            Mutation::Cas {
                key,
                expected,
                value: input.value(limits)?,
            }
        }
    };
    if !input.rest.is_empty() {
        return Err(DecodeError::TrailingBytes {
            count: input.rest.len(),
        });
    }
    Ok(Command {
        request_id,
        mutation,
    })
}

/// A cursor over the bytes still to decode.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// The next `len` bytes, which belong to `field`.
    fn take(&mut self, len: usize, field: Field) -> Result<&'a [u8], DecodeError> {
        let (head, rest) = self
            .rest
            .split_at_checked(len)
            .ok_or(DecodeError::Truncated { field })?;
        self.rest = rest;
        Ok(head)
    }

    /// The next `N` bytes, which belong to `field`.
    fn array<const N: usize>(&mut self, field: Field) -> Result<[u8; N], DecodeError> {
        self.take(N, field)?
            .try_into()
            .map_err(|_| DecodeError::Truncated { field })
    }

    /// The next byte, which is `field`.
    fn byte(&mut self, field: Field) -> Result<u8, DecodeError> {
        let [byte] = self.array::<1>(field)?;
        Ok(byte)
    }

    /// A UTF-8 `field` after its `length` prefix.
    fn text(&mut self, length: Field, field: Field) -> Result<String, DecodeError> {
        let len = u32::from_be_bytes(self.array(length)?);
        let len = usize::try_from(len).map_err(|_| DecodeError::Truncated { field })?;
        let bytes = self.take(len, field)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::NotUtf8 { field })
    }

    /// A value under `limits`.
    fn value(&mut self, limits: KeyValueLimits) -> Result<Value, DecodeError> {
        let value = self.text(Field::ValueLength, Field::Value)?;
        Value::parse(value, limits).map_err(DecodeError::Value)
    }

    /// A compare-and-swap's expectation.
    fn expectation(&mut self) -> Result<ExpectedVersion, DecodeError> {
        match self.byte(Field::Expectation)? {
            ABSENT => Ok(ExpectedVersion::Absent),
            AT_VERSION => {
                let raw = u64::from_be_bytes(self.array(Field::ExpectedVersion)?);
                Version::new(raw)
                    .map(ExpectedVersion::Version)
                    .map_err(DecodeError::ExpectedVersion)
            }
            tag => Err(DecodeError::UnknownExpectation { tag }),
        }
    }
}

#[cfg(test)]
mod tests {
    // check: COMMAND-CODEC

    use types::{COMMAND_ENVELOPE_BOUND, COMMAND_FORMAT_VERSION, MAX_SAFE_INTEGER};

    use super::*;

    fn key(text: &str) -> Key {
        Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
    }

    fn value(text: &str) -> Value {
        Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
    }

    fn command(id: u8, mutation: Mutation) -> Command {
        Command {
            request_id: RequestId::from_bytes([id; 16]),
            mutation,
        }
    }

    /// One command of every shape: each kind, both expectations, an empty value and
    /// multibyte UTF-8.
    fn samples() -> Vec<Command> {
        vec![
            command(
                1,
                Mutation::Put {
                    key: key("k"),
                    value: value("v1"),
                },
            ),
            command(
                2,
                Mutation::Put {
                    key: key("caf\u{e9}/\u{1f511}"),
                    value: Value::EMPTY,
                },
            ),
            command(3, Mutation::Delete { key: key("gone") }),
            command(
                4,
                Mutation::Cas {
                    key: key("k"),
                    expected: ExpectedVersion::Absent,
                    value: value("first"),
                },
            ),
            command(
                5,
                Mutation::Cas {
                    key: key("\u{3b1}"),
                    expected: ExpectedVersion::Version(Version::new(MAX_SAFE_INTEGER).unwrap()),
                    value: value("\u{e9}t\u{e9}"),
                },
            ),
        ]
    }

    #[test]
    fn every_command_shape_round_trips() {
        for original in samples() {
            let bytes = encode(&original);
            assert_eq!(
                decode(&bytes, KeyValueLimits::MAXIMA),
                Ok(original.clone()),
                "rule: decode(encode(x)) == x (§10.19); command={original:?} bytes={bytes:?}"
            );
        }
    }

    #[test]
    fn encodings_follow_the_v1_layout() {
        let id = [0xab; 16];
        let put = command(
            0xab,
            Mutation::Put {
                key: key("k"),
                value: value("v1"),
            },
        );
        let mut expected = vec![1, 1];
        expected.extend_from_slice(&id);
        expected.extend_from_slice(&[0, 0, 0, 1, b'k', 0, 0, 0, 2, b'v', b'1']);
        assert_eq!(encode(&put), expected, "rule: the v1 PUT layout is pinned");

        let delete = command(0xab, Mutation::Delete { key: key("k") });
        let mut expected = vec![1, 2];
        expected.extend_from_slice(&id);
        expected.extend_from_slice(&[0, 0, 0, 1, b'k']);
        assert_eq!(
            encode(&delete),
            expected,
            "rule: the v1 DELETE layout is pinned"
        );

        let cas = command(
            0xab,
            Mutation::Cas {
                key: key("k"),
                expected: ExpectedVersion::Version(Version::new(7).unwrap()),
                value: Value::EMPTY,
            },
        );
        let mut expected = vec![1, 3];
        expected.extend_from_slice(&id);
        expected.extend_from_slice(&[0, 0, 0, 1, b'k', 1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0]);
        assert_eq!(
            encode(&cas),
            expected,
            "rule: the v1 compare-and-swap layout is pinned"
        );

        let absent = command(
            0xab,
            Mutation::Cas {
                key: key("k"),
                expected: ExpectedVersion::Absent,
                value: Value::EMPTY,
            },
        );
        assert_eq!(
            encode(&absent).get(23),
            Some(&ABSENT),
            "rule: an absent expectation is the single byte 0, with no version"
        );
        assert_eq!(
            u64::from(FORMAT),
            COMMAND_FORMAT_VERSION,
            "rule: the format byte is the fingerprinted command-format version (DUR-8)"
        );
    }

    #[test]
    fn every_valid_command_fits_the_command_limit() {
        assert!(
            u64::try_from(MAX_ENVELOPE_BYTES).unwrap() <= COMMAND_ENVELOPE_BOUND,
            "rule: the codec's envelope fits the bound §7.10's cross-limit check assumes; \
             envelope={MAX_ENVELOPE_BYTES} bound={COMMAND_ENVELOPE_BOUND}"
        );
        for original in samples() {
            let payload = original.mutation.key().as_str().len()
                + original.mutation.value().map_or(0, |v| v.as_str().len());
            let envelope = encode(&original).len() - payload;
            assert!(
                envelope <= MAX_ENVELOPE_BYTES,
                "rule: every envelope is within MAX_ENVELOPE_BYTES; envelope={envelope} \
                 command={original:?}"
            );
        }
        let largest = command(
            0xff,
            Mutation::Cas {
                key: key(&"k".repeat(Key::MAX_BYTES)),
                expected: ExpectedVersion::Version(Version::new(MAX_SAFE_INTEGER).unwrap()),
                value: value(&"v".repeat(Value::MAX_BYTES)),
            },
        );
        let bytes = encode(&largest);
        assert_eq!(
            bytes.len(),
            MAX_ENVELOPE_BYTES + Key::MAX_BYTES + Value::MAX_BYTES,
            "rule: the largest command is a compare-and-swap with a version, a 256 B key and \
             a 64 KiB value"
        );
        assert!(
            u64::try_from(bytes.len()).unwrap() <= MAX_COMMAND_BYTES,
            "rule: every valid command fits the 66 KiB command limit (§7.11); largest={}",
            bytes.len()
        );
        assert_eq!(
            decode(&bytes, KeyValueLimits::MAXIMA),
            Ok(largest),
            "rule: the largest command round-trips"
        );
    }

    #[test]
    fn oversized_input_is_rejected_before_reading() {
        let limit = usize::try_from(MAX_COMMAND_BYTES).unwrap();
        assert_eq!(
            decode(&vec![1; limit + 1], KeyValueLimits::MAXIMA),
            Err(DecodeError::TooLarge { bytes: limit + 1 }),
            "rule: an input over 66 KiB is refused before parsing (§10.19)"
        );
        assert_ne!(
            decode(&vec![1; limit], KeyValueLimits::MAXIMA),
            Err(DecodeError::TooLarge { bytes: limit }),
            "rule: exactly 66 KiB is within the limit"
        );
    }

    #[test]
    fn malformed_inputs_are_typed_errors() {
        let put = encode(&samples()[0]);
        let with = |at: usize, byte: u8| {
            let mut bytes = put.clone();
            bytes[at] = byte;
            bytes
        };
        let mut cas = encode(&samples()[4]);
        let tag_at = 2 + 16 + 4 + "\u{3b1}".len();
        let cases: Vec<(&str, Vec<u8>, DecodeError)> = vec![
            (
                "empty input",
                Vec::new(),
                DecodeError::Truncated {
                    field: Field::Format,
                },
            ),
            (
                "another format version",
                with(0, 2),
                DecodeError::UnknownFormat { version: 2 },
            ),
            ("kind 0", with(1, 0), DecodeError::UnknownKind { kind: 0 }),
            ("kind 4", with(1, 4), DecodeError::UnknownKind { kind: 4 }),
            (
                "a short request ID",
                put[..10].to_vec(),
                DecodeError::Truncated {
                    field: Field::RequestId,
                },
            ),
            (
                "an empty key",
                with(21, 0),
                DecodeError::Key(KeyError::Empty),
            ),
            (
                "a key length beyond the input",
                with(20, 200),
                DecodeError::Truncated { field: Field::Key },
            ),
            (
                "a key that is not UTF-8",
                with(22, 0xff),
                DecodeError::NotUtf8 { field: Field::Key },
            ),
            (
                "a value that is not UTF-8",
                with(27, 0xc3),
                DecodeError::NotUtf8 {
                    field: Field::Value,
                },
            ),
            (
                "a trailing byte",
                [put.as_slice(), &[0]].concat(),
                DecodeError::TrailingBytes { count: 1 },
            ),
        ];
        for (case, bytes, expected) in cases {
            assert_eq!(
                decode(&bytes, KeyValueLimits::MAXIMA),
                Err(expected),
                "rule: {case} is a typed decode error; bytes={bytes:?}"
            );
        }

        cas[tag_at] = 2;
        assert_eq!(
            decode(&cas, KeyValueLimits::MAXIMA),
            Err(DecodeError::UnknownExpectation { tag: 2 }),
            "rule: the expectation byte is 0 or 1"
        );
        cas[tag_at] = AT_VERSION;
        for raw in [0, MAX_SAFE_INTEGER + 1] {
            cas.splice(tag_at + 1..tag_at + 9, raw.to_be_bytes());
            assert_eq!(
                decode(&cas, KeyValueLimits::MAXIMA),
                Err(DecodeError::ExpectedVersion(Version::new(raw).unwrap_err())),
                "rule: an expected version outside 1..=2^53-1 is refused (D-77); raw={raw}"
            );
        }
    }

    #[test]
    fn decoding_applies_the_configured_limits() {
        let limits = KeyValueLimits::new(16, 64).unwrap();
        let fits = command(
            9,
            Mutation::Put {
                key: key(&"k".repeat(16)),
                value: value(&"v".repeat(64)),
            },
        );
        assert_eq!(
            decode(&encode(&fits), limits),
            Ok(fits),
            "rule: a key and value at the configured limits decode (D-80)"
        );
        let long_key = command(
            9,
            Mutation::Put {
                key: key(&"k".repeat(17)),
                value: Value::EMPTY,
            },
        );
        assert_eq!(
            decode(&encode(&long_key), limits),
            Err(DecodeError::Key(KeyError::TooLong {
                bytes: 17,
                limit: 16
            })),
            "rule: a key over the configured max_key_bytes is refused (D-80)"
        );
        let long_value = command(
            9,
            Mutation::Put {
                key: key("k"),
                value: value(&"v".repeat(65)),
            },
        );
        assert_eq!(
            decode(&encode(&long_value), limits),
            Err(DecodeError::Value(ValueError::TooLong {
                bytes: 65,
                limit: 64
            })),
            "rule: a value over the configured max_value_bytes is refused (D-80)"
        );
    }

    /// A seeded `SplitMix64` step: these tests' only randomness (§7.14).
    fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Decoding `bytes` must not panic, and whatever it accepts must be canonical.
    fn assert_canonical_or_refused(bytes: &[u8], context: &str) {
        if let Ok(decoded) = decode(bytes, KeyValueLimits::MAXIMA) {
            assert_eq!(
                encode(&decoded),
                bytes,
                "rule: decode accepts only canonical bytes, so encode(decode(b)) == b; {context}"
            );
        }
    }

    #[test]
    fn damaged_encodings_never_panic_and_never_decode_non_canonically() {
        for original in samples() {
            let bytes = encode(&original);
            for cut in 0..bytes.len() {
                assert!(
                    decode(&bytes[..cut], KeyValueLimits::MAXIMA).is_err(),
                    "rule: a proper prefix of a command never decodes; cut={cut} \
                     command={original:?}"
                );
            }
            for at in 0..bytes.len() {
                for mask in [0x01, 0x80, 0xff] {
                    let mut damaged = bytes.clone();
                    damaged[at] ^= mask;
                    assert_canonical_or_refused(
                        &damaged,
                        &format!("byte {at} xor {mask:#04x} of {original:?}"),
                    );
                }
            }
        }
    }

    #[test]
    fn random_inputs_never_panic_and_never_decode_non_canonically() {
        const SEED: u64 = 0x5eed_c0de;
        let mut state = SEED;
        for case in 0..4_000 {
            let len = usize::try_from(next(&mut state) % 64).unwrap();
            let mut bytes: Vec<u8> = (0..len)
                .map(|_| next(&mut state).to_le_bytes()[0])
                .collect();
            // Steer half of the cases past the header, so the deeper fields are reached.
            if case % 2 == 0 && bytes.len() >= 2 {
                bytes[0] = FORMAT;
                bytes[1] = 1 + next(&mut state).to_le_bytes()[0] % 3;
            }
            assert_canonical_or_refused(
                &bytes,
                &format!("seed={SEED:#x} case={case} bytes={bytes:?}"),
            );
        }
    }
}
