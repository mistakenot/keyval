//! SHA-256 (FIPS 180-4), implemented in-crate so the deterministic core takes no hashing
//! dependency (PLAN.md §7.5, D-75, CD-12).
//!
//! It digests the semantic-config fingerprint's canonical encoding (DUR-8) and, from W09,
//! canonical commands (`command_digest`, DM-5). Collision resistance is a stated
//! assumption, not a proof (§10.15). The tests pin it to the FIPS 180-4 example vectors
//! and to independently computed digests at every padding boundary.

/// The SHA-256 round constants: the first 32 bits of the fractional parts of the cube
/// roots of the first 64 primes (FIPS 180-4 §4.2.2).
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The initial hash value: the first 32 bits of the fractional parts of the square roots
/// of the first 8 primes (FIPS 180-4 §5.3.3).
const INITIAL: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// A streaming SHA-256 hasher: [`update`](Self::update) with any split of the message,
/// then [`finalize`](Self::finalize).
///
/// Invariant: `buffered < 64`, and `buffer[..buffered]` holds the message bytes not yet
/// compressed; `length` counts every byte passed to `update`.
#[derive(Clone, Debug)]
pub struct Sha256 {
    /// The chaining state.
    state: [u32; 8],
    /// A partial block.
    buffer: [u8; 64],
    /// How many bytes of `buffer` are in use.
    buffered: usize,
    /// The message length so far, in bytes.
    length: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// A hasher for an empty message.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: INITIAL,
            buffer: [0; 64],
            buffered: 0,
            length: 0,
        }
    }

    /// The digest of `message`.
    #[must_use]
    pub fn digest(message: &[u8]) -> [u8; 32] {
        let mut hasher = Self::new();
        hasher.update(message);
        hasher.finalize()
    }

    /// Appends `bytes` to the message.
    pub fn update(&mut self, bytes: &[u8]) {
        self.length = self
            .length
            .wrapping_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let mut rest = bytes;
        if self.buffered > 0 {
            let take = (64 - self.buffered).min(rest.len());
            let (head, tail) = rest.split_at(take);
            if let Some(slot) = self.buffer.get_mut(self.buffered..self.buffered + take) {
                slot.copy_from_slice(head);
            }
            self.buffered += take;
            rest = tail;
            if self.buffered < 64 {
                // `bytes` did not fill the partial block, so nothing is left to process.
                return;
            }
            compress(&mut self.state, &self.buffer);
            self.buffered = 0;
        }
        let (blocks, tail) = rest.as_chunks::<64>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        if let Some(slot) = self.buffer.get_mut(..tail.len()) {
            slot.copy_from_slice(tail);
            self.buffered = tail.len();
        }
    }

    /// The digest of the message passed to [`update`](Self::update) so far.
    #[must_use]
    pub fn finalize(mut self) -> [u8; 32] {
        // FIPS 180-4 §5.1.1: a 1 bit, zeros to 56 bytes mod 64, the length in bits.
        let bit_length = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        let zeros = (64 + 56 - self.buffered) % 64;
        self.update(&[0; 64][..zeros]);
        self.update(&bit_length.to_be_bytes());
        let mut digest = [0; 32];
        for (out, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *out = word.to_be_bytes();
        }
        digest
    }
}

/// One application of the compression function to a 64-byte block (FIPS 180-4 §6.2.2).
#[allow(
    clippy::many_single_char_names,
    reason = "FIPS 180-4 names the working variables a to h; br-w08-2-semantic-fingerprint-pr9"
)]
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut schedule = [0_u32; 64];
    for (word, bytes) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    for t in 16..64 {
        let w15 = schedule[t - 15];
        let w2 = schedule[t - 2];
        let sigma0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
        let sigma1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
        schedule[t] = schedule[t - 16]
            .wrapping_add(sigma0)
            .wrapping_add(schedule[t - 7])
            .wrapping_add(sigma1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for (k, w) in K.into_iter().zip(schedule) {
        let big_sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(big_sigma1)
            .wrapping_add(choose)
            .wrapping_add(k)
            .wrapping_add(w);
        let big_sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_sigma0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(add);
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::Sha256;

    /// `digest` as lowercase hex.
    fn hex(digest: [u8; 32]) -> String {
        digest.iter().fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").unwrap();
            text
        })
    }

    #[test]
    fn the_fips_180_4_example_vectors() {
        let million_a = "a".repeat(1_000_000);
        let cases: [(&str, &str); 5] = [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
            (
                &million_a,
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                hex(Sha256::digest(message.as_bytes())),
                expected,
                "rule: SHA-256 matches FIPS 180-4 (D-75); message length {}",
                message.len()
            );
        }
    }

    #[test]
    fn every_padding_boundary_matches_an_independent_digest() {
        // Messages 0, 1, 2, … n-1 (bytes); digests computed independently with Python's
        // hashlib. Lengths 55/56 and 119/120 straddle the one-block padding limit; 63/64/65
        // and 127/128 straddle block boundaries.
        let message: Vec<u8> = (0..=255).collect();
        let cases: [(usize, &str); 9] = [
            (
                55,
                "463eb28e72f82e0a96c0a4cc53690c571281131f672aa229e0d45ae59b598b59",
            ),
            (
                56,
                "da2ae4d6b36748f2a318f23e7ab1dfdf45acdc9d049bd80e59de82a60895f562",
            ),
            (
                63,
                "29af2686fd53374a36b0846694cc342177e428d1647515f078784d69cdb9e488",
            ),
            (
                64,
                "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108",
            ),
            (
                65,
                "4bfd2c8b6f1eec7a2afeb48b934ee4b2694182027e6d0fc075074f2fabb31781",
            ),
            (
                119,
                "da18797ed7c3a777f0847f429724a2d8cd5138e6ed2895c3fa1a6d39d18f7ec6",
            ),
            (
                120,
                "f52b23db1fbb6ded89ef42a23ce0c8922c45f25c50b568a93bf1c075420bbb7c",
            ),
            (
                127,
                "92ca0fa6651ee2f97b884b7246a562fa71250fedefe5ebf270d31c546bfea976",
            ),
            (
                128,
                "471fb943aa23c511f6f72f8d1652d9c880cfa392ad80503120547703e56a2be5",
            ),
        ];
        for (length, expected) in cases {
            assert_eq!(
                hex(Sha256::digest(&message[..length])),
                expected,
                "rule: SHA-256 pads correctly at every boundary; length={length}"
            );
        }
    }

    #[test]
    fn any_split_of_the_message_gives_the_same_digest() {
        let message: Vec<u8> = (0..300_u16)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let whole = Sha256::digest(&message);
        for split in [1, 3, 55, 63, 64, 65, 127, 200] {
            let mut hasher = Sha256::new();
            for chunk in message.chunks(split) {
                hasher.update(chunk);
            }
            assert_eq!(
                hasher.finalize(),
                whole,
                "rule: streaming in chunks of {split} bytes changes nothing"
            );
        }
    }
}
