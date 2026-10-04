# Recovery golden vectors

One journal per recovery case in PLAN.md §7.6 "Golden vectors" (DUR-5, DUR-6, DUR-9).
`src/golden_recovery.rs` recovers each file and checks the outcome: a refusal with its
specific error, or the recovered log, hard state and torn-tail truncation.

Goldens are oracles (P9). A format change bumps the format version, adds new files and is
recorded in PLAN.md §16. A file here is never regenerated to make a test pass.

**Provenance.** Each file was built field by field from the layouts in `../README.md`, by
a separate script. Its checksums came from an independent bitwise CRC-32C, which was first
checked against the RFC 3720 vectors. Neither the bytes nor the checksums came from this
crate's encoder.

**Bodies.** Except `empty.bin` and `incomplete-genesis.bin`, which are whole journals,
each file is a journal **body**: the frames after the genesis, from sequence 2. The test
prepends the genesis frame of node 1 under `config/local.toml`. The files therefore do not
pin the semantic fingerprint, which `../genesis.bin` and the fingerprint tests own. Offsets
are relative to the body.

The body frames:
- **F2**, sequence 2, 113 bytes: commands `put a` and `put b` at indexes 1 and 2, term 1;
  hard state term 1, vote 1, commit 0.
- **F3**, sequence 3, 87 bytes: command `put c` at index 3, term 1; hard state term 1,
  vote 1, commit 2.
- **F4**, sequence 4, 114 bytes: commands `put c2` and `put d` at indexes 3 and 4, term 2,
  replacing index 3; hard state term 2, vote 3, commit 3.

| File | What it represents |
|---|---|
| `empty.bin` | An empty journal: refused (D-52), and only `init` may run |
| `incomplete-genesis.bin` | The first 60 bytes of `../genesis.bin` and nothing after it: a crash interrupted `init`. Refused, and only `init` may re-run (§7.7 step 0) |
| `clean.bin` | F2 and F3: recovers entries 1–3 and hard state term 1, vote 1, commit 2, with no torn tail |
| `logical-truncation.bin` | F2, F3 and F4: a logical truncation followed by replacement entries; recovers `put c2` at index 3 and `put d` at 4 in term 2 |
| `torn-mid-header.bin` | `clean.bin` plus the first 10 bytes of F4, cut mid-header: the torn tail is discarded |
| `torn-mid-payload.bin` | `clean.bin` plus the first 44 bytes of F4, cut mid-payload with an intact header: discarded |
| `torn-mid-trailer.bin` | `clean.bin` plus F4 less its last 3 bytes, cut mid-trailer: discarded |
| `bad-checksum-final.bin` | `clean.bin` plus F4 complete but with its last payload byte damaged: a bad checksum on the final frame, discarded as a torn tail (D-50) |
| `zero-tail.bin` | `clean.bin` plus 64 zero bytes: discarded |
| `garbage-tail.bin` | `clean.bin` plus 64 bytes of `GARBAGE!`, which has bad magic: discarded |
| `crash-during-truncation.bin` | `torn-mid-payload.bin` partly truncated, to the first 5 bytes of F4: recovery converges on the same answer |
| `bad-checksum-mid.bin` | F2 with a payload byte damaged, then a valid F3: refused as corruption through the resync scan (D-60) |
| `corrupt-header-then-valid.bin` | F2, then F3 with a damaged header byte, then a valid F4: frame N+1 is corrupt and a valid N+2 follows, so it is refused |
| `sequence-gap.bin` | F2 then F4, with no sequence 3: a sequence gap with a valid continuation, refused |
| `tail-too-long.bin` | `clean.bin` plus one maximum frame and a byte of zeros (1,048,609 bytes): a trailing region longer than the crash model allows, refused |
| `truncation-below-commit.bin` | `clean.bin` (commit 2) plus a sequence-4 frame replacing index 2: a truncation below the commit, refused (DUR-6) |
| `impossible-version-final.bin` | `clean.bin` plus F4 with format version 2 and both checksums recomputed: an intact, deliberately written header this build never writes, refused (never a torn tail; DUR-9, br-o69w) |
| `commit-regression.bin` | `clean.bin` (commit 2) plus a sequence-4 frame with no entries and hard state term 1, vote 1, commit 1: a persisted commit going back, an impossible history, refused (br-n60a, §7.6) |
