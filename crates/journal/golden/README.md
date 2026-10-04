# Journal golden vectors

Committed byte files that pin the journal format (PLAN.md §7.6, DUR-5). Each test in
`src/golden.rs` checks two things:
- decoding the file gives the expected record;
- encoding the expected record gives identical bytes.

Goldens are oracles (P9). A format change bumps the format version, adds new files and is
recorded in PLAN.md §16. A file here is never regenerated to make a test pass.

**Provenance.** Each file was built field by field from the layouts below, by a separate
script. Its checksums came from an independent bitwise CRC-32C, which was first checked
against the RFC 3720 vectors. Neither the bytes nor the checksums came from this crate's
encoder.

| File | What it represents |
|---|---|
| `genesis.bin` | The genesis frame (sequence 1) for node 1 of a 5-voter cluster: instance ID `00112233445566778899aabbccddeeff`, cluster ID `keyval-local-v1`, voters 1–5. The semantic fingerprint is the fixed 32 bytes `3b2e3ed3…c26c`: `config/local.toml`'s fingerprint under encoding v1. The golden pins the frame format, not the fingerprint algorithm |
| `raft-batch.bin` | A Raft batch (sequence 2): an empty entry at index 1 and a command entry at index 2 (`golden command bytes`), both in term 1, with the hard state term 1, vote 1, commit 1 |
| `raft-batch-entry-only.bin` | A Raft batch (sequence 3): one 1-byte command entry (`x`) at index 3, term 2, no hard state. It has the smallest per-entry overhead: a 26-byte envelope |
| `raft-batch-vote-only.bin` | A Raft batch (sequence 4): no entries, hard state term 3, no vote, commit 2. This is a term or vote change, which must sync before any message that depends on it (DUR-2) |

## Frame layout (all integers little-endian)

| Offset | Field | Size |
|---|---|---|
| 0 | magic `KVJ1` | 4 |
| 4 | format version: 1 | 2 |
| 6 | kind: 1 genesis, 2 Raft batch | 1 |
| 7 | reserved: 0 | 1 |
| 8 | payload length | 4 |
| 12 | frame sequence | 8 |
| 20 | header checksum: CRC-32C of bytes 0..20 | 4 |
| 24 | payload | length |
| 24 + length | checksum: CRC-32C of bytes 0..24 + length | 4 |
| 28 + length | trailing length, equal to the payload length | 4 |

## Genesis payload

The fields, in order:
1. format version: u16, 1;
2. instance ID: 16 bytes;
3. cluster ID length: u8;
4. cluster ID: UTF-8;
5. node ID: u64;
6. voter count: u8, 1, 3 or 5;
7. voter IDs: u64 each, strictly ascending;
8. semantic fingerprint: 32 bytes.

## Raft-batch payload

It starts with the entry count, a u32. Each entry then has these fields, in order:
1. index: u64;
2. term: u64;
3. payload kind: u8, 0 for empty or 1 for command;
4. data length: u32;
5. data.

Last comes the hard-state flag, a u8: 0 means absent and 1 present. When it is present,
three u64 values follow: the term, the vote (0 means none) and the commit index.
