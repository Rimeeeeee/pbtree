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

`NodeDatabase::pin_roots` returns a lease that prevents root removal across handles
without holding `mutation`. Release the mutation lock before acquiring a client
database writer. Clients must preserve external block references and marker files
for pinned roots too. `generation` and `invalidate_caches` coordinate client caches
when references or roots are removed.

`collect_cooperatively` marks without blocking checkpoint writers and yields between
bounded sweep batches. Saves during a sweep repair and mark reused subtrees before
publishing a root. Interrupted sweeps recover before the next ordinary save.

`NodeDatabase::proof` reads and authenticates a single native search path while a
mutation guard or root lease protects it. It avoids full-state reconstruction.
Native persistence benchmarks include these path reads. `load` still reconstructs
the complete in-memory state.

Keep `DecodeLimits::default()` for untrusted migration artifacts. Locally executed
native state can use `DecodeLimits::native()` independently, allowing valid chain
growth beyond import budgets. `StateTree::estimated_memory_size` supports cache
budgets without scanning accounts or storage.

```sh
cargo test -p pbt
cargo test -p pbt --all-features
cargo clippy -p pbt --all-features --all-targets -- -D warnings
```

With `parallel` enabled, call
`BinaryTree::from_sorted_entries_parallel` to build sorted leaves using Rayon.
It returns the usual `BinaryTree` with the same root and proof format. Small
inputs use the serial builder; callers can select a Rayon thread pool.
`StateTree::apply_batch_parallel`, `BinaryTree::apply_updates_parallel`, and
`replay::replay_parallel` also use parallel hashing for dense batches. Sparse
updates keep incremental path copying; roots and validation rules match the serial APIs.

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
cargo test --features parallel
```

Coverage-guided tree and artifact fuzzing runs in CI. Local commands and input
formats are documented in [fuzz/README.md](fuzz/README.md).


Rust 1.96 or newer is required for this package. Native persistence also requires
a C++ compiler and libclang. Both EIPs are drafts; BLAKE3 is a reference configuration, not a finalized network
hash selection. Conversion/restore still materialize full state in memory.
This extraction does not establish mainnet-scale performance or production readiness.

## Benchmarks

The [CodSpeed workflow](.github/workflows/codspeed.yml) runs all six suites on
pull requests, pushes to `main`, and manual runs. See the
[CodSpeed dashboard](https://codspeed.io/Rimeeeeee/pbtree) for results and
comparisons once the repository is connected and `main` has a baseline run.

CI uses CPU simulation to track CPU work. Use local wall-clock timings to measure
RocksDB I/O and the elapsed-time benefit of parallel execution.

```sh
cargo bench --locked --benches                                     # tree and state
cargo bench --locked --benches --all-features                      # all six suites
cargo bench --locked --features parallel --bench backends          # serial vs. Rayon
```

The full suite adds migration, BAL replay, RocksDB persistence, and parallel
construction. The persistence suite requires a C++ compiler and libclang.
For a local check of the CodSpeed harness on Linux:

```sh
cargo install cargo-codspeed --version 6.0.0 --locked
cargo codspeed build --locked --all-features -m simulation
cargo codspeed run -m simulation
```

The last command runs the benchmarks locally to verify the harness. GitHub
Actions runs them under CodSpeed's simulator and uploads the measurements.

The [November 11 wall-clock run](benches/results/current-2026-11-11.json) is
archived for reference.
