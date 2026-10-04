//! The journal frame (PLAN.md §7.6): one length-delimited, checksummed record.
//!
//! Layout, all integers little-endian:
//!
//! | Offset | Field | Size |
//! |---|---|---|
//! | 0 | magic `KVJ1` | 4 |
//! | 4 | format version | 2 |
//! | 6 | kind (1 genesis, 2 Raft batch) | 1 |
//! | 7 | reserved, zero | 1 |
//! | 8 | payload length, at most 1 MiB | 4 |
//! | 12 | frame sequence | 8 |
//! | 20 | header checksum: CRC-32C over bytes 0..20 | 4 |
//! | 24 | payload | length |
//! | 24 + length | checksum: CRC-32C over bytes 0..24 + length (header and payload) | 4 |
//! | 28 + length | trailing length, equal to the payload length | 4 |
//!
//! The header checksum lets a recovery scan trust an intact header's length (D-69), so
//! [`decode_header`] stands alone. The fixed fields add up to `types::FRAME_OVERHEAD_BYTES`
//! (32 B), the per-frame part of the journal margin formula (§7.10).

use core::fmt;

use types::MAX_FRAME_PAYLOAD_BYTES;

use crate::crc32c::crc32c;

/// The frame magic.
pub const MAGIC: [u8; 4] = *b"KVJ1";

/// The frame format version this build writes and reads (DUR-5). A format change bumps
/// it, adds new golden vectors and is recorded in §16; goldens are never regenerated.
pub const FORMAT_VERSION: u16 = 1;

/// The header's length: every fixed field before the payload.
pub const HEADER_LEN: usize = 24;

/// The trailer's length: the checksum and the trailing length.
pub const TRAILER_LEN: usize = 8;

/// The length of the header fields the header checksum covers.
const HEADER_CHECKED_LEN: usize = 20;

/// What a frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// The genesis record: always the first frame (§7.6, DUR-8).
    Genesis,
    /// One persisted Ready: entries and an optional hard state (§7.4 step 3).
    RaftBatch,
}

impl FrameKind {
    /// The kind's byte in the header.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Genesis => 1,
            Self::RaftBatch => 2,
        }
    }

    /// The kind with this byte, if any.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Genesis),
            2 => Some(Self::RaftBatch),
            _ => None,
        }
    }
}

/// A frame header whose checksum and fields are valid.
///
/// Invariant: `payload_len` is at most `types::MAX_FRAME_PAYLOAD_BYTES` (1 MiB, §7.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// What the frame carries.
    kind: FrameKind,
    /// The payload's length in bytes.
    payload_len: u32,
    /// The frame sequence.
    sequence: u64,
}

impl Header {
    /// What the frame carries.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        self.kind
    }

    /// The payload's length in bytes.
    #[must_use]
    pub const fn payload_len(&self) -> u32 {
        self.payload_len
    }

    /// The frame sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The whole frame's length in bytes: header, payload and trailer.
    #[must_use]
    pub const fn frame_len(&self) -> usize {
        HEADER_LEN + self.payload_len as usize + TRAILER_LEN
    }
}

/// A complete, valid frame, borrowing its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The frame's header.
    header: Header,
    /// The frame's payload.
    payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// The frame's header.
    #[must_use]
    pub const fn header(&self) -> Header {
        self.header
    }

    /// The frame's payload.
    #[must_use]
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

/// Why bytes are not a valid frame (§7.6 "Fail closed"; W11.3 decides what each means).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Fewer bytes than the header or the frame needs.
    Incomplete {
        /// The bytes needed, from the frame's start.
        needed: usize,
        /// The bytes available.
        available: usize,
    },
    /// The first four bytes are not `KVJ1`.
    BadMagic,
    /// The header checksum does not match the header's fields.
    HeaderChecksum {
        /// The stored checksum.
        stored: u32,
        /// The checksum of the stored fields.
        computed: u32,
    },
    /// A format version this build does not read.
    UnsupportedVersion {
        /// The stored version.
        version: u16,
    },
    /// A kind byte that names no record.
    UnknownKind {
        /// The stored byte.
        code: u8,
    },
    /// The reserved byte is not zero.
    NonZeroReserved {
        /// The stored byte.
        value: u8,
    },
    /// A payload longer than `types::MAX_FRAME_PAYLOAD_BYTES` (1 MiB, §7.11).
    PayloadTooLong {
        /// The payload's length.
        len: u64,
    },
    /// Sequence 0, which no frame carries: the genesis is 1 and batches follow it.
    ZeroSequence,
    /// The frame checksum does not match the header and payload.
    Checksum {
        /// The stored checksum.
        stored: u32,
        /// The checksum of the stored header and payload.
        computed: u32,
    },
    /// The trailing length differs from the header's payload length.
    TrailingLength {
        /// The header's payload length.
        expected: u32,
        /// The trailing length.
        found: u32,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { needed, available } => {
                write!(f, "incomplete frame: {available} of {needed} bytes")
            }
            Self::BadMagic => f.write_str("bad frame magic: not KVJ1"),
            Self::HeaderChecksum { stored, computed } => write!(
                f,
                "header checksum mismatch: stored {stored:#010x}, computed {computed:#010x}"
            ),
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported frame format version {version}")
            }
            Self::UnknownKind { code } => write!(f, "unknown frame kind {code}"),
            Self::NonZeroReserved { value } => write!(f, "reserved byte is {value}, not 0"),
            Self::PayloadTooLong { len } => {
                write!(
                    f,
                    "payload of {len} bytes exceeds {MAX_FRAME_PAYLOAD_BYTES}"
                )
            }
            Self::ZeroSequence => f.write_str("frame sequence 0, which no frame carries"),
            Self::Checksum { stored, computed } => write!(
                f,
                "frame checksum mismatch: stored {stored:#010x}, computed {computed:#010x}"
            ),
            Self::TrailingLength { expected, found } => {
                write!(
                    f,
                    "trailing length {found} differs from payload length {expected}"
                )
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl FrameError {
    /// Whether the header was intact, its checksum valid, yet held a field no frame this
    /// build writes can have. A torn append is a prefix of a frame this build wrote, so this
    /// is never crash damage: recovery refuses it (DUR-9, br-o69w). The other errors mean
    /// no intact header, or a frame cut short or failing its frame checksum.
    #[must_use]
    pub const fn header_intact(&self) -> bool {
        match self {
            Self::UnsupportedVersion { .. }
            | Self::UnknownKind { .. }
            | Self::NonZeroReserved { .. }
            | Self::PayloadTooLong { .. }
            | Self::ZeroSequence => true,
            Self::Incomplete { .. }
            | Self::BadMagic
            | Self::HeaderChecksum { .. }
            | Self::Checksum { .. }
            | Self::TrailingLength { .. } => false,
        }
    }
}

/// The `N` bytes at `at`, if present.
fn bytes_at<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes
        .get(at..at.checked_add(N)?)
        .and_then(|slice| slice.try_into().ok())
}

/// The little-endian `u32` at `at`, or 0 if absent (callers check lengths first).
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    bytes_at(bytes, at).map_or(0, u32::from_le_bytes)
}

/// Encodes one frame.
///
/// # Errors
///
/// [`FrameError::PayloadTooLong`] if `payload` exceeds 1 MiB.
pub fn encode(kind: FrameKind, sequence: u64, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    let payload_len = u32::try_from(len)
        .ok()
        .filter(|_| len <= MAX_FRAME_PAYLOAD_BYTES)
        .ok_or(FrameError::PayloadTooLong { len })?;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len() + TRAILER_LEN);
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    frame.push(kind.code());
    frame.push(0);
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(&sequence.to_le_bytes());
    let header_checksum = crc32c(&frame);
    frame.extend_from_slice(&header_checksum.to_le_bytes());
    frame.extend_from_slice(payload);
    let checksum = crc32c(&frame);
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    Ok(frame)
}

/// Decodes and validates the header at the start of `bytes`, without reading the
/// payload. A valid header checksum makes its length trustworthy (D-69).
///
/// # Errors
///
/// The first [`FrameError`] found, checked in this order: fewer than [`HEADER_LEN`]
/// bytes, bad magic, header checksum, version, kind, reserved byte, payload length.
pub fn decode_header(bytes: &[u8]) -> Result<Header, FrameError> {
    let Some(header) = bytes.get(..HEADER_LEN) else {
        return Err(FrameError::Incomplete {
            needed: HEADER_LEN,
            available: bytes.len(),
        });
    };
    if bytes_at::<4>(header, 0) != Some(MAGIC) {
        return Err(FrameError::BadMagic);
    }
    let stored = u32_at(header, HEADER_CHECKED_LEN);
    let computed = crc32c(header.get(..HEADER_CHECKED_LEN).unwrap_or_default());
    if stored != computed {
        return Err(FrameError::HeaderChecksum { stored, computed });
    }
    let version = bytes_at(header, 4).map_or(0, u16::from_le_bytes);
    if version != FORMAT_VERSION {
        return Err(FrameError::UnsupportedVersion { version });
    }
    let code = header.get(6).copied().unwrap_or_default();
    let kind = FrameKind::from_code(code).ok_or(FrameError::UnknownKind { code })?;
    let reserved = header.get(7).copied().unwrap_or_default();
    if reserved != 0 {
        return Err(FrameError::NonZeroReserved { value: reserved });
    }
    let payload_len = u32_at(header, 8);
    if u64::from(payload_len) > MAX_FRAME_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLong {
            len: u64::from(payload_len),
        });
    }
    let sequence = bytes_at(header, 12).map_or(0, u64::from_le_bytes);
    if sequence == 0 {
        return Err(FrameError::ZeroSequence);
    }
    Ok(Header {
        kind,
        payload_len,
        sequence,
    })
}

/// Decodes and validates the complete frame at the start of `bytes`. Bytes after the
/// frame are ignored; [`Header::frame_len`] says where the next frame starts.
///
/// # Errors
///
/// Any [`decode_header`] error, then [`FrameError::Incomplete`] if the frame is cut
/// short, [`FrameError::Checksum`] or [`FrameError::TrailingLength`].
pub fn decode(bytes: &[u8]) -> Result<Frame<'_>, FrameError> {
    let header = decode_header(bytes)?;
    let frame_len = header.frame_len();
    let Some(frame) = bytes.get(..frame_len) else {
        return Err(FrameError::Incomplete {
            needed: frame_len,
            available: bytes.len(),
        });
    };
    let checked_len = frame_len - TRAILER_LEN;
    let stored = u32_at(frame, checked_len);
    let computed = crc32c(frame.get(..checked_len).unwrap_or_default());
    if stored != computed {
        return Err(FrameError::Checksum { stored, computed });
    }
    let found = u32_at(frame, checked_len + 4);
    if found != header.payload_len {
        return Err(FrameError::TrailingLength {
            expected: header.payload_len,
            found,
        });
    }
    Ok(Frame {
        header,
        payload: frame.get(HEADER_LEN..checked_len).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    // check: JOURNAL-FRAMES

    use super::*;
    use types::{FRAME_FIELDS, FRAME_OVERHEAD_BYTES};

    /// A valid frame with a 5-byte payload.
    fn sample() -> Vec<u8> {
        encode(FrameKind::RaftBatch, 7, b"hello").unwrap()
    }

    /// `frame` with the byte at `at` replaced by `value`, and the header checksum fixed
    /// up, so only the field under test is wrong.
    fn with_header_byte(mut frame: Vec<u8>, at: usize, value: u8) -> Vec<u8> {
        frame[at] = value;
        let checksum = crc32c(&frame[..HEADER_CHECKED_LEN]);
        frame[HEADER_CHECKED_LEN..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
        frame
    }

    #[test]
    fn the_layout_is_the_one_w08_pinned() {
        let sizes: Vec<u64> = FRAME_FIELDS.iter().map(|(_, size)| *size).collect();
        let header: u64 = sizes[..7].iter().sum();
        let trailer: u64 = sizes[7..].iter().sum();
        assert_eq!(
            (
                u64::try_from(HEADER_LEN).unwrap(),
                u64::try_from(TRAILER_LEN).unwrap()
            ),
            (header, trailer),
            "rule: the frame layout is §7.6's, as types::FRAME_FIELDS pins it (W08)"
        );
        assert_eq!(
            u64::try_from(HEADER_LEN + TRAILER_LEN).unwrap(),
            FRAME_OVERHEAD_BYTES,
            "rule: 32 B of overhead per frame, the margin formula's frame share (§7.10)"
        );
        assert_eq!(sample().len(), HEADER_LEN + 5 + TRAILER_LEN);
    }

    #[test]
    fn a_frame_round_trips() {
        for (kind, sequence, payload) in [
            (FrameKind::Genesis, 1, b"genesis".to_vec()),
            (FrameKind::RaftBatch, u64::MAX, Vec::new()),
            (FrameKind::RaftBatch, 2, vec![0xab; 1000]),
        ] {
            let bytes = encode(kind, sequence, &payload).unwrap();
            let frame = decode(&bytes).unwrap();
            assert_eq!(
                (
                    frame.header().kind(),
                    frame.header().sequence(),
                    frame.payload()
                ),
                (kind, sequence, payload.as_slice()),
                "rule: decode(encode(x)) == x (§7.6)"
            );
            assert_eq!(frame.header().frame_len(), bytes.len());
            let mut followed = bytes.clone();
            followed.extend_from_slice(b"next frame");
            assert_eq!(
                decode(&followed),
                Ok(frame),
                "rule: bytes after a frame are ignored"
            );
        }
    }

    #[test]
    fn the_largest_payload_fits_and_one_byte_more_does_not() {
        let max = usize::try_from(MAX_FRAME_PAYLOAD_BYTES).unwrap();
        let bytes = encode(FrameKind::RaftBatch, 1, &vec![0; max]).unwrap();
        assert_eq!(decode(&bytes).map(|frame| frame.payload().len()), Ok(max));
        assert_eq!(
            encode(FrameKind::RaftBatch, 1, &vec![0; max + 1]),
            Err(FrameError::PayloadTooLong {
                len: MAX_FRAME_PAYLOAD_BYTES + 1
            }),
            "rule: a frame payload is at most 1 MiB (§7.11)"
        );
        let mut oversized = sample();
        let claimed = u32::try_from(MAX_FRAME_PAYLOAD_BYTES + 1).unwrap();
        oversized[8..12].copy_from_slice(&claimed.to_le_bytes());
        let oversized = with_header_byte(oversized.clone(), 8, oversized[8]);
        assert_eq!(
            decode_header(&oversized),
            Err(FrameError::PayloadTooLong {
                len: MAX_FRAME_PAYLOAD_BYTES + 1
            }),
            "rule: a header claiming more than 1 MiB is refused even with a valid checksum"
        );
    }

    #[test]
    fn every_damaged_field_is_refused_with_its_own_error() {
        let good = sample();
        let len = good.len();
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        let mut header_flip = good.clone();
        header_flip[13] ^= 1; // a sequence byte, under the header checksum
        let mut payload_flip = good.clone();
        payload_flip[HEADER_LEN] ^= 1;
        let mut trailing = good.clone();
        trailing[len - 4] = 4;
        let cases = [
            (
                good[..10].to_vec(),
                FrameError::Incomplete {
                    needed: 24,
                    available: 10,
                },
            ),
            (
                good[..len - 1].to_vec(),
                FrameError::Incomplete {
                    needed: len,
                    available: len - 1,
                },
            ),
            (bad_magic, FrameError::BadMagic),
            (
                header_flip.clone(),
                FrameError::HeaderChecksum {
                    stored: u32_at(&header_flip, 20),
                    computed: crc32c(&header_flip[..20]),
                },
            ),
            (
                with_header_byte(good.clone(), 4, 2),
                FrameError::UnsupportedVersion { version: 2 },
            ),
            (
                with_header_byte(good.clone(), 6, 9),
                FrameError::UnknownKind { code: 9 },
            ),
            (
                with_header_byte(good.clone(), 7, 1),
                FrameError::NonZeroReserved { value: 1 },
            ),
            (
                payload_flip.clone(),
                FrameError::Checksum {
                    stored: u32_at(&payload_flip, HEADER_LEN + 5),
                    computed: crc32c(&payload_flip[..HEADER_LEN + 5]),
                },
            ),
            (
                trailing,
                FrameError::TrailingLength {
                    expected: 5,
                    found: 4,
                },
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                decode(&bytes).map(|frame| frame.header()),
                Err(expected),
                "rule: a damaged frame is refused with a specific error (§7.6 fail closed)"
            );
        }
    }

    #[test]
    fn a_header_decodes_without_its_payload() {
        let good = sample();
        let header = decode_header(&good[..HEADER_LEN]).unwrap();
        assert_eq!(
            (
                header.kind(),
                header.payload_len(),
                header.sequence(),
                header.frame_len()
            ),
            (FrameKind::RaftBatch, 5, 7, good.len()),
            "rule: an intact header alone gives a trustworthy length (D-69)"
        );
    }
}
