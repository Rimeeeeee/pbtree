# pbt

Client-independent experimental Partitioned Binary Tree implementation for
[EIP-8297](https://eips.ethereum.org/EIPS/eip-8297) and canonical migration/replay
primitives for [EIP-8347](https://eips.ethereum.org/EIPS/eip-8347).

## Features

- Default: canonical persistent binary tree, proofs, Ethereum state embedding,
  shared code, inline/overflow storage, delegation, and zero-as-deletion.
- `migration`: canonical tagged snapshot/preimage artifacts and independent
  MPT/PBT root authentication using the published `alloy-trie` crate.
- `bal`: authenticated EIP-7928 post-value replay and bounded discardable forks.
- `persistence`: content-addressed RocksDB trie nodes, synced incremental writes,
  verified cold restoration, and registered-root garbage collection.
- `experimental`: explicit parallel bulk construction and an append-only arena
  backend for measuring allocation and cache tradeoffs. Disabled by default.

Reth-specific execution bundles, retained-state cursors, finality/header checks,
atomic checkpoint files, node startup, staged backfill and RPC remain in Reth.
No Reth crates are dependencies of this library.

## Using the library

```rust
use pbt::{AccountData, Blake3Hasher, StateTree};
use alloy_primitives::{Address, U256};

let mut state = StateTree::<Blake3Hasher>::new();
state.set_account(Address::ZERO, AccountData {
    nonce: 1,
    balance: U256::from(10),
    code: Default::default(),
})?;
let commitment = state.root();
# Ok::<(), pbt::StateError>(())
```

Native database users must serialize composite operations with
`NodeDatabase::mutation`: write nodes/root records first, durably publish their
own block references afterwards, and remove references before unregistering roots
or collecting nodes. The database directory must already exist.

```sh
cargo test -p pbt
cargo test -p pbt --all-features
cargo clippy -p pbt --all-features --all-targets -- -D warnings
```

Run the tree and state benchmarks with `cargo bench`. Enable the optional
migration, BAL replay, and persistence benchmarks with `cargo bench --all-features`.

With `experimental` enabled, call
`BinaryTree::from_sorted_entries_parallel` to build sorted leaves using Rayon.
It returns the usual `BinaryTree` with the same root and proof format. Small
inputs use the serial builder; callers can select a Rayon thread pool.
`experimental::ArenaTree` is a separate API: snapshots share append-only storage,
writers share a lock, and historical nodes remain allocated until the arena is
released. `compact()` detaches one snapshot into fresh storage. Enabling the
feature keeps the existing `BinaryTree`, `StateTree`, and database APIs in use.

```sh
cargo test --features experimental
cargo bench --features experimental --bench experimental
```

Coverage-guided tree and artifact fuzzing runs in CI. Local commands and input
formats are documented in [fuzz/README.md](fuzz/README.md).


Rust 1.96 or newer is required for this package. Native persistence also requires
a C++ compiler and libclang. Both EIPs are drafts; BLAKE3 is a reference configuration, not a finalized network
hash selection. Conversion/restore still materialize full state in memory.
This extraction does not establish mainnet-scale performance or production readiness.

## Benchmarks

Measured on 2026-11-11 at `e3fe443`, using an AMD Ryzen 9 8940HX under Ubuntu
WSL2, `rustc 1.97.0-nightly (d3cd04068 2026-05-16)`, release mode, and all features.
These are mean times with 10 samples, 0.5 s warm-up, and a 1 s target measurement.
[All 53 results and their 95% confidence intervals](benches/results/local-2026-11-11.json)
include the full environment and commit ID.

| Operation | Fixture | Mean time |
|---|---|---:|
| Sorted tree build | 16,384 leaves | 10.8 ms |
| Incremental tree build | 16,384 leaves | 86.2 ms |
| Leaf replacement | 16,384 leaves | 4.34 us |
| Leaf removal | 16,384 leaves | 4.12 us |
| Membership proof generation | 16,384 leaves | 271 ns |
| Membership proof verification | 16,384 leaves | 3.85 us |
| Account balance update | 2,048 accounts | 16.6 us |
| Overflow storage replacement | 2,048 accounts | 6.54 us |
| Artifact encoding | 2,048 accounts | 59.6 ms |
| MPT anchor verification | 2,048 accounts | 174 ms |
| PBT checkpoint restoration | 2,048 accounts | 153 ms |
| Authenticated BAL replay | 256 changed accounts | 10.8 ms |
| Midpoint finalization | 4,096 layers | 1.8 ms |
| Synced save, one account changed | 4,096 accounts | 2.32 ms |
| Synced save, unchanged state | 4,096 accounts | 2.11 ms |
| Verified state reconstruction, warm DB cache | 4,096 accounts | 474 ms |

Fixture construction is excluded. Persistence saves include synchronous WAL
writes; reconstruction starts with a fresh in-memory state while RocksDB/OS
caches may be warm. Benchmark source and fixture details are in `benches/`.

Every CI run measures the change and its base on the same runner. Results update
one PR comment, or a commit comment for pushes to `main`. Regressions and
improvements are flagged above 5% when the 95% confidence intervals do not overlap;
these flags are advisory. Changed harnesses/toolchains and added or removed cases
are reported separately. Full results are retained as the `benchmark-results`
workflow artifact for 30 days. Local snapshot times are independent of CI hardware.
