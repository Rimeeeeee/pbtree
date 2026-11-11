# Fuzzing

Requires nightly Rust and `cargo-fuzz` 0.13.2. Run from the repository root:

```sh
cargo install cargo-fuzz --version 0.13.2 --locked
mkdir -p fuzz/corpus/tree_ops fuzz/corpus/artifact_decode
cargo +nightly fuzz run tree_ops fuzz/corpus/tree_ops fuzz/seeds/tree_ops -- -max_total_time=60 -timeout=10 -rss_limit_mb=2048 -max_len=16384
cargo +nightly fuzz run artifact_decode fuzz/corpus/artifact_decode fuzz/seeds/artifact_decode -- -max_total_time=60 -timeout=10 -rss_limit_mb=2048 -max_len=65536
cargo +nightly fuzz run tree_ops --features experimental fuzz/corpus/tree_ops fuzz/seeds/tree_ops -- -max_total_time=60 -timeout=10 -rss_limit_mb=2048 -max_len=16384
```

AddressSanitizer is enabled by default. New coverage inputs go to `fuzz/corpus/`;
the checked-in seed directories are read-only inputs to libFuzzer. Crash,
timeout, and out-of-memory reproducers go to `fuzz/artifacts/<target>/`. Replay
one with `cargo +nightly fuzz run <target> <path-to-reproducer>`.

- `tree_ops`: bounded insert/replace/remove/read sequences with variable-length
  keys, prefix conflicts, zero values, snapshots, iteration, and tampered proofs.
  Roots are compared with an independently encoded reference tree. Fourteen
  seeds cover varied mutations, repeated keys, prefix conflicts in both orders,
  branch collapse, empty reads, truncated records, and all-zero/all-one values. Keys are
  limited to eight bytes and sequences to 128 operations; deep/max-length keys
  are covered by the regular tree regression tests.
- `artifact_decode`: mutations of canonical snapshot/preimage pairs. Restoration
  against fixed PBT roots must agree with independent MPT authentication, and
  accepted artifacts must re-encode byte-for-byte. Eight seeds cover empty state,
  account headers, zero code, PUSH data spanning chunks, 257 code chunks,
  delegation, shared code, and inline/overflow storage including `U256::MAX`.
  Every run also checks that all eight original seeds are accepted and twelve
  curated malformed artifacts are rejected (truncation, trailing bytes, unknown
  tags, missing preimages, and commitment/preimage mismatches).

These seeds are the generated cases used in the original local fuzz campaigns;
they are separate from the independent EELS vectors under `tests/testdata/`.
The artifact input is a one-byte fixture selector, a four-byte little-endian
snapshot length, snapshot bytes, then preimage bytes. Each tree operation uses
up to 42 bytes: control, key length, eight key bytes, then 32 value bytes.
Regenerate the 22 edge-case seeds with `python3 fuzz/generate-seeds.py`; CI checks
that these generated inputs match the checked-in files.

CI runs each target for 60 seconds on pushes to `main` and pull requests. A
failure fails the job; logs, discovered corpus inputs, and reproducers are saved
as workflow artifacts for 14 days. The fuzz package has its own committed lockfile.
An additional `tree_ops --features experimental` job compares arena mutations,
snapshots, compaction, and proofs against the default tree and independent model.
