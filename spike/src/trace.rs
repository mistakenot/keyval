//! The spike-local canonical trace: one ASCII line per event, in the order the simulation
//! produced them. It carries no host data: no paths, wall-clock times, PIDs or addresses.
//! Message payloads are the canonical protobuf bytes, in hex.

use std::fmt::Write as _;

/// An append-only event trace.
#[derive(Debug, Default)]
pub struct Trace {
    bytes: Vec<u8>,
}

impl Trace {
    /// Records one event of `node` (incarnation `inc`) at virtual tick `now`.
    pub fn event(&mut self, now: u64, node: u64, inc: u64, text: &str) {
        let mut line = String::with_capacity(text.len() + 24);
        let _ = write!(line, "t={now:04} n={node} i={inc} {text}");
        line.push('\n');
        self.bytes.extend_from_slice(line.as_bytes());
    }

    /// Records a cluster-wide line, such as the final statistics.
    pub fn note(&mut self, text: &str) {
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(b'\n');
    }

    /// The trace's bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// FNV-1a (64-bit) of `bytes`: a short, stable fingerprint for reports. It is not a
/// security property; traces are always compared in full.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_canonical_lines() {
        let mut trace = Trace::default();
        trace.event(7, 2, 1, "ROLE Leader term=3");
        trace.note("END ok");
        assert_eq!(
            String::from_utf8(trace.into_bytes()).unwrap(),
            "t=0007 n=2 i=1 ROLE Leader term=3\nEND ok\n"
        );
    }

    #[test]
    fn hex_and_fingerprint_are_stable() {
        assert_eq!(hex(&[0x00, 0xab, 0x10]), "00ab10");
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
