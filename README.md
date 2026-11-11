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
- `parallel`: Rayon bulk construction of sorted trees. Disabled by default.
- `arena`: append-only tree storage with compact node indices. Disabled by default.

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

For bulk writes, `BinaryTree::apply_updates` accepts `(key, Some(value))` writes
and `(key, None)` deletions, with the last update winning for repeated keys.
Explicit zero values remain leaves in the generic tree. `StateTree::apply_batch`
accepts ordered `StateUpdate` account, storage, and storage-clear operations;
zero storage values delete slots, and absent or empty account updates clear the
account. Both methods leave the original tree unchanged if the batch is invalid.

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

With `parallel` enabled, call
`BinaryTree::from_sorted_entries_parallel` to build sorted leaves using Rayon.
It returns the usual `BinaryTree` with the same root and proof format. Small
inputs use the serial builder; callers can select a Rayon thread pool.
With `arena` enabled, `arena::ArenaTree` provides a separate API: snapshots share append-only storage,
writers share a lock, and historical nodes remain allocated until the arena is
released. `compact()` detaches one snapshot into fresh storage. Enabling the
feature keeps the existing `BinaryTree`, `StateTree`, and database APIs in use.

Enable the feature in a consuming crate:

```toml
[dependencies]
pbt = { git = "https://github.com/Rimeeeeee/pbtree", features = ["parallel"] }
alloy-primitives = "1.6.1"
```

Then select the parallel builder explicitly:

```rust
use alloy_primitives::B256;
use pbt::{BinaryTree, Blake3Hasher};

// Keys must be strictly ascending, unique, and prefix-free.
let entries = (0u32..16_384)
    .map(|index| (index.to_be_bytes().to_vec(), B256::repeat_byte(1)));
let tree = BinaryTree::<Blake3Hasher>::from_sorted_entries_parallel(entries).unwrap();
assert_eq!(tree.len(), 16_384);
```

```sh
cargo test --features parallel,arena
cargo bench --features parallel,arena --bench backends
```

Coverage-guided tree and artifact fuzzing runs in CI. Local commands and input
formats are documented in [fuzz/README.md](fuzz/README.md).


Rust 1.96 or newer is required for this package. Native persistence also requires
a C++ compiler and libclang. Both EIPs are drafts; BLAKE3 is a reference configuration, not a finalized network
hash selection. Conversion/restore still materialize full state in memory.
This extraction does not establish mainnet-scale performance or production readiness.

## Benchmarks

Measured on 2026-11-11, comparing `4a83606` with `9af52b8` on the same AMD
Ryzen 9 8940HX under Ubuntu WSL2, using
`rustc 1.97.0-nightly (d3cd04068 2026-05-16)`, release mode, and all features.
These are mean times with 10 samples, 0.5 s warm-up, and a 1 s target measurement.
[All 53 baseline and 70 selected results, with 95% confidence intervals](benches/results/optimizations-2026-11-11.json)
include the full environment and source commits. Lower time is better; small
differences can be noise. Percentage changes use the unrounded mean times.

| Operation | Fixture | Before | After | Change |
|---|---|---:|---:|---:|
| Sorted tree build | 16,384 leaves | 11.0 ms | 10.6 ms | -3.4% |
| Incremental tree build | 16,384 leaves | 89.5 ms | 74.2 ms | -17.2% |
| Leaf replacement | 16,384 leaves | 4.47 us | 3.89 us | -12.9% |
| Leaf removal | 16,384 leaves | 4.14 us | 3.70 us | -10.7% |
| Membership proof generation | 16,384 leaves | 304 ns | 273 ns | -10.3% |
| Membership proof verification | 16,384 leaves | 4.72 us | 3.26 us | -30.8% |
| Account balance update | 2,048 accounts | 16.9 us | 6.08 us | -64.1% |
| Overflow storage replacement | 2,048 accounts | 7.24 us | 5.94 us | -18.0% |
| Artifact encoding | 2,048 accounts | 72.6 ms | 8.83 ms | -87.8% |
| MPT anchor verification | 2,048 accounts | 206 ms | 71.2 ms | -65.5% |
| PBT checkpoint restoration | 2,048 accounts | 174 ms | 53.9 ms | -69.1% |
| Authenticated BAL replay | 64 changed accounts | 2.70 ms | 1.57 ms | -41.9% |
| Authenticated BAL replay | 256 changed accounts | 11.7 ms | 4.94 ms | -57.7% |
| Midpoint finalization | 4,096 layers | 1.82 ms | 1.80 ms | -1.2% |
| Synced save, one account changed | 4,096 accounts | 2.93 ms | 2.15 ms | -26.7% |
| Synced save, unchanged state | 4,096 accounts | 2.18 ms | 1.77 ms | -19.0% |
| Verified state reconstruction, warm DB cache | 4,096 accounts | 546 ms | 200 ms | -63.3% |

Fixture construction is excluded. Persistence saves include synchronous WAL
writes; reconstruction starts with a fresh in-memory state while RocksDB/OS
caches may be warm. Benchmark source and fixture details are in `benches/`.

The selected changes cache derived hashes, avoid unchanged writes, batch dense
leaf updates, and batch verified database reads. Sparse updates retain incremental
path copying. Public method signatures, commitment/proof formats, canonical
artifacts, and the native database format are preserved.

The optional parallel builder measured 8.30 ms versus 42.0 ms for serial
construction at 65,536 leaves with the default Rayon pool. At 16,384
leaves, arena reads measured 114 ns versus 81.6 ns, and replacements measured
3.98 us versus 3.73 us for the default tree. Arena replacement timings exclude
periodic compaction; it retains append history between compactions. The arena
therefore remains an explicit experimental backend.

The results also include two interleaved base/head repeats of membership proof
generation and 64/256-account replay (20 samples), plus 1,024-layer midpoint
finalization (10 samples), each with a 2 s target measurement.
The earlier 64-account replay regression was removed by keeping sparse batches
incremental. The original proof-generation implementation is retained; its
timings varied across repeats. A borderline 5% finalization regression in the
full run did not consistently reproduce in these repeats. The
[earlier 53-case snapshot](benches/results/local-2026-11-11.json) is retained
separately.

Every CI run measures the change and its base on the same runner. Results update
one PR comment, or a commit comment for pushes to `main`. Regressions and
improvements are flagged above 5% when the 95% confidence intervals do not overlap;
these flags are advisory. Changed harnesses/toolchains and added or removed cases
are reported separately. Full results are retained as the `benchmark-results`
workflow artifact for 30 days. Local snapshot times are independent of CI hardware.
