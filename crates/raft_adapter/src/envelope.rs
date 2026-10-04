//! The peer envelope (PLAN.md §8.2, D-63, D-71). Every Raft message travels between nodes
//! in one, and every envelope a node receives passes the identity check before Raft sees its
//! message.
//!
//! The simulated network (W16) and the HTTP peer transport (W21) carry exactly these bytes,
//! sealed and opened by this code (P3, T-2).
//!
//! # Wire format
//!
//! Integers are little-endian, as in the journal (§7.6). An envelope is exactly
//! [`HEADER_BYTES`] + *n* bytes.
//!
//! | Offset | Bytes | Field |
//! |---|---|---|
//! | 0 | 4 | magic `KVPE` |
//! | 4 | 8 | protocol version ([`PROTOCOL_VERSION`]) |
//! | 12 | 16 | cluster instance ID (§7.7 step 0) |
//! | 28 | 32 | semantic-config fingerprint (§7.10) |
//! | 60 | 8 | sender node ID |
//! | 68 | 8 | destination node ID |
//! | 76 | 4 | payload length *n* |
//! | 80 | *n* | the Raft message's protobuf bytes, at most [`MAX_FRAME_PAYLOAD_BYTES`] |
//!
//! # Opening
//!
//! [`PeerCodec::open`] checks, in this order:
//! 1. the length bound ([`MAX_ENVELOPE_BYTES`]) and the magic;
//! 2. the protocol version;
//! 3. the header, and a payload length that matches the bytes received;
//! 4. the instance ID;
//! 5. the fingerprint;
//! 6. the sender: another voter, and the node the transport received the envelope from;
//! 7. the destination: this node;
//! 8. the Raft message inside, which must be one v1 peers exchange ([`Inadmissible`]);
//! 9. the message's own sender, which must be the envelope's (its destination is this
//!    node by step 8).
//!
//! The first failure drops the envelope, and [`PeerCodec::dropped`] counts it by reason, so
//! two accidentally connected clusters never exchange Raft traffic (§8.2).

use types::{
    InstanceId, MAX_FRAME_PAYLOAD_BYTES, NodeId, OutboundMessage, PROTOCOL_VERSION,
    SemanticFingerprint,
};

use crate::admit::{Inadmissible, admit};

/// The envelope's first four bytes.
const MAGIC: [u8; 4] = *b"KVPE";

/// The header's length: every byte before the payload.
pub const HEADER_BYTES: usize = 80;

/// The longest envelope: the header plus a payload bounded like a journal frame's (§7.11).
/// A replication batch (at most 256 KiB, plus one entry) always fits.
pub const MAX_ENVELOPE_BYTES: u64 = HEADER_BYTES as u64 + MAX_FRAME_PAYLOAD_BYTES;

/// Why an identity could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// The node is not one of the voters.
    NotAVoter {
        /// The node.
        node: NodeId,
    },
}

/// Why a message could not be sealed. Neither happens for a message the library sends:
/// it addresses only other voters, and its batches are bounded (§7.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealError {
    /// The destination is not another voter.
    Destination {
        /// The destination.
        to: NodeId,
    },
    /// The message is longer than [`MAX_FRAME_PAYLOAD_BYTES`].
    TooLarge {
        /// The message's length in bytes.
        len: usize,
    },
}

/// Why a received envelope was dropped (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    /// Longer than [`MAX_ENVELOPE_BYTES`].
    TooLarge {
        /// The envelope's length in bytes.
        len: u64,
    },
    /// Not an envelope: the wrong magic, a short header, or a payload length that does not
    /// match the bytes received.
    Malformed,
    /// Another protocol version.
    ProtocolVersion {
        /// The version the envelope names.
        version: u64,
    },
    /// Another cluster instance.
    Instance,
    /// Another semantic configuration.
    Fingerprint,
    /// The sender is not another voter, or not the node the transport received the
    /// envelope from.
    Sender {
        /// The sender the envelope names.
        sender: u64,
    },
    /// Addressed to another node.
    Destination {
        /// The destination the envelope names.
        destination: u64,
    },
    /// The Raft message inside is not one v1 peers exchange.
    Message(Inadmissible),
    /// The Raft message's own sender is not the envelope's.
    Binding {
        /// The sender the Raft message names.
        from: u64,
    },
}

/// Dropped envelopes, counted by reason (§8.2, CD-9). Each count saturates at `u64::MAX`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DropCounts {
    /// [`EnvelopeError::TooLarge`].
    pub too_large: u64,
    /// [`EnvelopeError::Malformed`].
    pub malformed: u64,
    /// [`EnvelopeError::ProtocolVersion`].
    pub protocol_version: u64,
    /// [`EnvelopeError::Instance`].
    pub instance: u64,
    /// [`EnvelopeError::Fingerprint`].
    pub fingerprint: u64,
    /// [`EnvelopeError::Sender`].
    pub sender: u64,
    /// [`EnvelopeError::Destination`].
    pub destination: u64,
    /// [`EnvelopeError::Message`].
    pub message: u64,
    /// [`EnvelopeError::Binding`].
    pub binding: u64,
}

impl DropCounts {
    /// Every dropped envelope, whatever the reason.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.too_large
            .saturating_add(self.malformed)
            .saturating_add(self.protocol_version)
            .saturating_add(self.instance)
            .saturating_add(self.fingerprint)
            .saturating_add(self.sender)
            .saturating_add(self.destination)
            .saturating_add(self.message)
            .saturating_add(self.binding)
    }

    /// Counts one envelope dropped for `error`.
    const fn record(&mut self, error: EnvelopeError) {
        let count = match error {
            EnvelopeError::TooLarge { .. } => &mut self.too_large,
            EnvelopeError::Malformed => &mut self.malformed,
            EnvelopeError::ProtocolVersion { .. } => &mut self.protocol_version,
            EnvelopeError::Instance => &mut self.instance,
            EnvelopeError::Fingerprint => &mut self.fingerprint,
            EnvelopeError::Sender { .. } => &mut self.sender,
            EnvelopeError::Destination { .. } => &mut self.destination,
            EnvelopeError::Message(_) => &mut self.message,
            EnvelopeError::Binding { .. } => &mut self.binding,
        };
        *count = count.saturating_add(1);
    }
}

/// One node's end of the peer channel: its identity, which it seals into every envelope
/// it sends and checks on every envelope it receives, and its count of dropped envelopes.
///
/// Invariants: `voters` is sorted, without duplicates, and contains `node`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCodec {
    /// The cluster instance, from the genesis frame (§7.7 step 2).
    instance: InstanceId,
    /// The semantic-config fingerprint, verified against the genesis frame at startup.
    fingerprint: SemanticFingerprint,
    /// This node.
    node: NodeId,
    /// The static voter set (§7.7 step 6).
    voters: Vec<NodeId>,
    /// Envelopes dropped so far.
    dropped: DropCounts,
}

impl PeerCodec {
    /// The codec of node `node` among `voters` in cluster `instance`, whose semantic
    /// configuration has `fingerprint`.
    ///
    /// # Errors
    ///
    /// [`IdentityError::NotAVoter`] if `node` is not among `voters`.
    pub fn new(
        instance: InstanceId,
        fingerprint: SemanticFingerprint,
        node: NodeId,
        voters: impl IntoIterator<Item = NodeId>,
    ) -> Result<Self, IdentityError> {
        let mut voters: Vec<NodeId> = voters.into_iter().collect();
        voters.sort_unstable();
        voters.dedup();
        if voters.binary_search(&node).is_err() {
            return Err(IdentityError::NotAVoter { node });
        }
        Ok(Self {
            instance,
            fingerprint,
            node,
            voters,
            dropped: DropCounts::default(),
        })
    }

    /// This node.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// The envelopes dropped so far, by reason.
    #[must_use]
    pub const fn dropped(&self) -> DropCounts {
        self.dropped
    }

    /// `message`, sealed in an envelope from this node to its destination.
    ///
    /// # Errors
    ///
    /// [`SealError`] if the destination is not another voter, or the message is longer
    /// than [`MAX_FRAME_PAYLOAD_BYTES`].
    pub fn seal(&self, message: &OutboundMessage) -> Result<Vec<u8>, SealError> {
        if message.to == self.node || self.voters.binary_search(&message.to).is_err() {
            return Err(SealError::Destination { to: message.to });
        }
        let len = message.bytes.len();
        let payload_len = u32::try_from(len)
            .ok()
            .filter(|&payload_len| u64::from(payload_len) <= MAX_FRAME_PAYLOAD_BYTES)
            .ok_or(SealError::TooLarge { len })?;
        let mut envelope = Vec::with_capacity(HEADER_BYTES.saturating_add(len));
        envelope.extend_from_slice(&MAGIC);
        envelope.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        envelope.extend_from_slice(&self.instance.to_bytes());
        envelope.extend_from_slice(&self.fingerprint.to_bytes());
        envelope.extend_from_slice(&self.node.get().to_le_bytes());
        envelope.extend_from_slice(&message.to.get().to_le_bytes());
        envelope.extend_from_slice(&payload_len.to_le_bytes());
        envelope.extend_from_slice(&message.bytes);
        Ok(envelope)
    }

    /// The Raft message in `envelope`, which the transport received from `from`, if it
    /// passes every check (module documentation). A dropped envelope is counted.
    ///
    /// # Errors
    ///
    /// [`EnvelopeError`] names the first check the envelope failed.
    pub fn open(&mut self, from: NodeId, envelope: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        let opened = self.check(from, envelope);
        if let Err(error) = opened {
            self.dropped.record(error);
        }
        opened.map(<[u8]>::to_vec)
    }

    /// The payload of `envelope`, received from `from`, if it passes every check.
    fn check<'a>(&self, from: NodeId, envelope: &'a [u8]) -> Result<&'a [u8], EnvelopeError> {
        let len = u64::try_from(envelope.len()).unwrap_or(u64::MAX);
        if len > MAX_ENVELOPE_BYTES {
            return Err(EnvelopeError::TooLarge { len });
        }
        let mut reader = Reader(envelope);
        if reader.take::<4>() != Some(MAGIC) {
            return Err(EnvelopeError::Malformed);
        }
        let version = reader.u64()?;
        if version != PROTOCOL_VERSION {
            return Err(EnvelopeError::ProtocolVersion { version });
        }
        let instance = InstanceId::from_bytes(reader.take().ok_or(EnvelopeError::Malformed)?);
        let fingerprint =
            SemanticFingerprint::from_bytes(reader.take().ok_or(EnvelopeError::Malformed)?);
        let sender = reader.u64()?;
        let destination = reader.u64()?;
        let payload_len = reader
            .take()
            .map(u32::from_le_bytes)
            .ok_or(EnvelopeError::Malformed)?;
        let payload = reader.0;
        if usize::try_from(payload_len).ok() != Some(payload.len()) {
            return Err(EnvelopeError::Malformed);
        }
        if instance != self.instance {
            return Err(EnvelopeError::Instance);
        }
        if fingerprint != self.fingerprint {
            return Err(EnvelopeError::Fingerprint);
        }
        let known_peer = sender != self.node.get()
            && sender == from.get()
            && self.voters.iter().any(|voter| voter.get() == sender);
        if !known_peer {
            return Err(EnvelopeError::Sender { sender });
        }
        if destination != self.node.get() {
            return Err(EnvelopeError::Destination { destination });
        }
        let message = admit(payload, self.node, &self.voters).map_err(EnvelopeError::Message)?;
        if message.from != sender {
            return Err(EnvelopeError::Binding { from: message.from });
        }
        Ok(payload)
    }
}

/// The unread rest of an envelope.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    /// The next `N` bytes, or `None` if fewer remain.
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    /// The next little-endian `u64`.
    fn u64(&mut self) -> Result<u64, EnvelopeError> {
        self.take()
            .map(u64::from_le_bytes)
            .ok_or(EnvelopeError::Malformed)
    }
}
