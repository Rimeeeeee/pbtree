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
`StateTree::apply_batch_parallel`, `BinaryTree::apply_updates_parallel`, and
`replay::replay_parallel` also use parallel hashing for dense batches. Sparse
updates keep incremental path copying; roots and validation rules match the serial APIs.
With `arena` enabled, `pbt::arena::ArenaTree` provides a separate API: snapshots
share append-only storage, writers share a lock, and historical nodes remain
allocated until the arena is released. `compact()` detaches one snapshot into
fresh storage. Enabling either feature keeps the existing `BinaryTree`, `StateTree`,
and database APIs in use.

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

Measured on 2026-11-11 at `a3484da`. Mean times in release mode,
using BLAKE3 and all features. Each case uses **50 samples**, a 1 s warm-up,
and a 5 s target measurement. [All 93 results and 95% confidence intervals](benches/results/current-2026-11-11.json).

- CPU: **AMD Ryzen 9 8940HX, 16 cores / 32 threads**.
- OS: Ubuntu 24.04.5 LTS (WSL2), Linux 6.18.58.1.
- Memory available to the benchmark environment: 15.2 GiB.
- Compiler: `rustc 1.97.0-nightly (d3cd04068 2026-05-16)`.
- Parallel builder: 32 Rayon worker threads.

### Default library

Tree cases use Ethereum header keys. State fixtures have four storage slots per account.

| Operation | Fixture | Mean time |
|---|---|---:|
| Sorted tree build | 262,144 leaves | 180 ms |
| Incremental tree build | 262,144 leaves | 2.05 s |
| Leaf replacement | 262,144 leaves | 4.93 us |
| Leaf removal | 262,144 leaves | 4.69 us |
| Membership proof generation | 262,144 leaves | 322 ns |
| Membership proof verification | 262,144 leaves | 4.34 us |
| Absence proof generation | 262,144 leaves | 453 ns |
| Absence proof verification | 262,144 leaves | 4.86 us |
| Account balance update | 16,384 accounts | 8 us |
| Inline storage replacement | 16,384 accounts | 7.72 us |
| Overflow storage replacement | 16,384 accounts | 7.31 us |
| Overflow storage deletion | 16,384 accounts | 6.06 us |
| Account removal, shared code | 16,384 accounts | 37.1 us |
| Artifact encoding | 16,384 accounts | 106 ms |
| MPT anchor verification | 16,384 accounts | 781 ms |
| PBT checkpoint restoration | 16,384 accounts | 598 ms |
| Authenticated BAL replay | 64 changed / 16,384 parent accounts | 2.04 ms |
| Authenticated BAL replay | 256 changed / 16,384 parent accounts | 8.48 ms |
| Authenticated BAL replay | 4,096 changed / 16,384 parent accounts | 102 ms |
| Finalization, same anchor | 16,384 layers | 354 ns |
| Midpoint finalization | 16,384 layers | 9 ms |
| Tip finalization | 16,384 layers | 6.4 ms |
| Synced save, one account changed | 16,384 accounts | 3.11 ms |
| Synced save, unchanged state | 16,384 accounts | 2.26 ms |
| Verified state reconstruction, warm DB cache | 16,384 accounts | 1.11 s |

### Parallel builder and arena

These cases use identical sorted 32-byte keys for every backend.

| Sorted build | Default tree | `parallel` | `arena` |
|---|---:|---:|---:|
| 16,384 leaves | 10.7 ms | 4.26 ms | 10.7 ms |
| 65,536 leaves | 43.1 ms | 8.83 ms | 42.3 ms |
| 262,144 leaves | 181 ms | 23.2 ms | 177 ms |

| Operation | Leaves | Default tree | `arena` |
|---|---:|---:|---:|
| Lookup | 16,384 | 87.1 ns | 123 ns |
| Replacement | 16,384 | 4.03 us | 4.23 us |
| Lookup | 262,144 | 148 ns | 192 ns |
| Replacement | 262,144 | 6.01 us | 1.96 ms |

Fixture setup is untimed. Point reads and writes target a fixed key/account.
Persistence saves include synchronous WAL writes and exclude root removal and GC;
state reconstruction uses a fresh in-memory state with potentially warm database
and OS caches. Arena replacement timings exclude periodic compaction but include
any node-vector growth on the next write.
