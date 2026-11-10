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

Coverage-guided tree and artifact fuzzing runs in CI. Local commands and input
formats are documented in [fuzz/README.md](fuzz/README.md).


Rust 1.96 or newer is required for this package. Native persistence also requires
a C++ compiler and libclang. Both EIPs are drafts; BLAKE3 is a reference configuration, not a finalized network
hash selection. Conversion/restore still materialize full state in memory.
This extraction does not establish mainnet-scale performance or production readiness.
