//! CRC-32C (Castagnoli), implemented in-crate so the journal takes no checksum
//! dependency (PLAN.md §7.6, CD-12).
//!
//! It detects accidental damage, not adversaries (§7.6, D-69). The tests pin it to the
//! RFC 3720 appendix B.4 vectors and the standard check value.

/// The reflected Castagnoli polynomial, 0x1EDC6F41 bit-reversed.
const POLYNOMIAL: u32 = 0x82F6_3B78;

/// The byte-at-a-time lookup table, built at compile time.
const TABLE: [u32; 256] = table();

/// The lookup table: the CRC of each byte value.
const fn table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut byte: u32 = 0;
    while byte < 256 {
        let mut crc = byte;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[byte as usize] = crc;
        byte += 1;
    }
    table
}

/// The CRC-32C of `crc`'s message followed by `bytes`; start from 0 for a new message.
/// `update(update(0, a), b) == crc32c(a ++ b)`.
#[must_use]
pub fn update(crc: u32, bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!crc, |crc, &byte| {
        TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

/// The CRC-32C of `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    update(0, bytes)
}

#[cfg(test)]
mod tests {
    use super::{crc32c, update};

    #[test]
    fn the_standard_vectors() {
        // RFC 3720 B.4 and the check value; recomputed independently by a bitwise
        // implementation when the goldens were built.
        let ascending: Vec<u8> = (0..32).collect();
        let descending: Vec<u8> = (0..32).rev().collect();
        let iscsi_read = [
            0x01, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14,
            0x00, 0x00, 0x00, 0x18, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let cases: [(&str, &[u8], u32); 7] = [
            ("empty", &[], 0),
            ("check value", b"123456789", 0xe306_9283),
            ("32 zero bytes", &[0; 32], 0x8a91_36aa),
            ("32 0xff bytes", &[0xff; 32], 0x62a8_ab43),
            ("32 ascending bytes", &ascending, 0x46dd_794e),
            ("32 descending bytes", &descending, 0x113f_db5c),
            ("iSCSI read command PDU", &iscsi_read, 0xd996_3a56),
        ];
        for (name, bytes, expected) in cases {
            assert_eq!(
                crc32c(bytes),
                expected,
                "rule: CRC-32C matches RFC 3720 (§7.6, CD-12); vector={name}"
            );
        }
    }

    #[test]
    fn updating_in_pieces_equals_one_pass() {
        let message: Vec<u8> = (0..=255).cycle().take(1000).collect();
        let whole = crc32c(&message);
        for split in [0, 1, 7, 64, 999, 1000] {
            let (head, tail) = message.split_at(split);
            assert_eq!(
                update(update(0, head), tail),
                whole,
                "rule: the CRC composes across pieces; split={split}"
            );
        }
    }
}
