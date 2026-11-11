//! Partitioned binary state commitment and canonical migration artifacts.
//!
//! Implements the experimental EIP-8297 embedding and EIP-8347 artifact/replay rules.
//! BLAKE3 is the reference configuration; the proposals have not finalized their hash choice.
//! Client execution, header/finality validation and checkpoint publication remain client-owned.

mod hash;
mod prefix;
pub use hash::{Blake3Hasher, TreeHasher};

mod tree;
pub use tree::{BinaryTree, MAX_KEY_LENGTH, TreeError};

mod proof;
pub use proof::{Proof, ProofError, ProofStep, ProofTerminal};

/// Zone-separated Ethereum state key derivation and value encoding.
pub mod key;

mod state;
pub use state::{AccountData, StateError, StateTree, StateUpdate};

/// Opt-in arena storage for comparing allocation/cache tradeoffs with the reference tree.
#[cfg(feature = "experimental")]
pub mod experimental;

/// Canonical snapshot/preimage artifacts, bounded decoding, and dual-root verification.
#[cfg(feature = "migration")]
pub mod migration;

/// Authenticated block access-list replay and discardable fork layers.
#[cfg(feature = "bal")]
pub mod replay;

/// Incremental native trie nodes; clients publish their own block/checkpoint references.
#[cfg(feature = "persistence")]
pub mod persistence;
