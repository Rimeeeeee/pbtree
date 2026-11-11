//! Content-addressed trie nodes shared by durable fork checkpoints.

use crate::{
    StateTree, TreeHasher,
    hash::{branch_hash, leaf_hash},
    key::{self, ACCOUNT_KEY_LENGTH, CODE_HASH_LEAF_KEY, DELEGATION_LEAF_KEY, STORAGE_KEY_LENGTH},
    migration::{ArtifactError, DecodeLimits, Reader, restore_leaves},
    tree::Node,
};
use alloy_primitives::{Address, B256, U256};
use rocksdb::{DB, Options, WriteBatch, WriteOptions};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

pub use rocksdb;

/// Content-addressed native trie storage shared across handles to the same directory.
///
/// Clients own checkpoint/reference publication and lock `mutation` across composite operations.
/// Native node hashes are verified on load; cold restores currently materialize full state.
#[derive(Debug)]
pub struct NodeDatabase {
    db: DB,
    /// Serializes checkpoint publication, reads, root removal, and garbage collection.
    /// Hold this lock across composite operations that publish external block references.
    pub mutation: Mutex<()>,
}

impl NodeDatabase {
    /// Opens the native database below an existing directory, sharing its process-local lock.
    /// Upgrades version 2 stores, recovering orphan subtrees before the next save.
    /// Unknown schema versions and inaccessible directories are errors.
    pub fn open(path: &Path) -> Result<Arc<Self>, NodeError> {
        // RocksDB owns an exclusive file lock. Reopening a store in this process must share it.
        static DATABASES: OnceLock<Mutex<BTreeMap<PathBuf, Weak<NodeDatabase>>>> = OnceLock::new();
        let path = path
            .canonicalize()
            .map_err(|source| NodeError::Io { path: path.to_path_buf(), source })?;
        let mut databases =
            DATABASES.get_or_init(Mutex::default).lock().map_err(|_| NodeError::Poisoned)?;
        databases.retain(|_, database| database.strong_count() != 0);
        if let Some(database) = databases.get(&path).and_then(Weak::upgrade) {
            return Ok(database);
        }
        let mut options = Options::default();
        options.create_if_missing(true);
        let db = DB::open_cf(&options, path.join("nodes"), ["default"])?;
        match db.get(b"schema")? {
            Some(version) if version.as_slice() == STORE_VERSION.to_be_bytes() => {}
            Some(version) if version.as_slice() != 2u32.to_be_bytes() => {
                return Err(ArtifactError::Invalid("unsupported PBT database version").into());
            }
            _ => {
                // A legacy store may contain orphan parents whose children were
                // swept before a crash. Recover before trusting existing subtrees.
                let mut batch = WriteBatch::default();
                batch.put(b"schema", STORE_VERSION.to_be_bytes());
                batch.put(GC_PENDING, []);
                write(&db, batch)?;
            }
        }
        let database = Arc::new(Self { db, mutation: Mutex::default() });
        databases.insert(path, Arc::downgrade(&database));
        Ok(database)
    }

    /// Returns a registered root's leaf count, or `None` when no record exists.
    /// Serialize access with `mutation` if roots can be removed concurrently.
    pub fn leaf_count(&self, root: B256) -> Result<Option<usize>, NodeError> {
        self.db
            .get(database_key(ROOT, root))?
            .map(|value| {
                let count = value
                    .as_slice()
                    .try_into()
                    .map_err(|_| ArtifactError::Invalid("invalid PBT root record"))?;
                usize::try_from(u64::from_be_bytes(count))
                    .map_err(|_| ArtifactError::ResourceLimit.into())
            })
            .transpose()
    }

    /// Durably saves new trie paths and registers the root after its children reach the WAL.
    /// Callers hold `mutation` until any external checkpoint reference is also published.
    /// Invalid state or exhausted decoding limits return an error before root publication.
    pub fn save<H: TreeHasher>(
        &self,
        state: &StateTree<H>,
        limits: DecodeLimits,
    ) -> Result<(), NodeError> {
        if state.account_count() > limits.max_accounts || state.tree().len() > limits.max_leaves {
            return Err(ArtifactError::ResourceLimit.into());
        }
        let (empty_accounts, total_code, max_code) = state.validation_stats();
        if empty_accounts != 0 {
            return Err(ArtifactError::Invalid("empty anchor account").into());
        }
        if max_code > limits.max_code_size || total_code > limits.max_total_code_size as u128 {
            return Err(ArtifactError::ResourceLimit.into());
        }
        // A partial sweep can leave an unregistered parent after deleting its children.
        // Finish collection before save can mistake that parent for a complete subtree.
        if self.db.get(GC_PENDING)?.is_some() {
            self.collect::<H>()?;
        }
        let mut stack: Vec<_> =
            state.tree().root.as_deref().map(|node| (node, false)).into_iter().collect();
        let mut batch = WriteBatch::default();
        while let Some((node, expanded)) = stack.pop() {
            let hash = node.hash();
            if !expanded && self.db.get_pinned(database_key(NODE, hash))?.is_some() {
                continue;
            }
            // Publish children before parents so a cancelled multi-batch save can be retried.
            if !expanded && let Node::Branch { left, right, .. } = node {
                stack.push((node, true));
                stack.push((right, false));
                stack.push((left, false));
                continue;
            }
            let mut encoded = Vec::new();
            match node {
                Node::Leaf { key, value, .. } => {
                    encoded.push(LEAF);
                    encoded.extend_from_slice(
                        &u16::try_from(key.len())
                            .map_err(|_| ArtifactError::ResourceLimit)?
                            .to_be_bytes(),
                    );
                    encoded.extend_from_slice(key);
                    encoded.extend_from_slice(value.as_slice());
                    if key[0] == key::ACCOUNT_ZONE
                        && matches!(key[33], CODE_HASH_LEAF_KEY | DELEGATION_LEAF_KEY)
                    {
                        let address = state
                            .address_preimage(B256::from_slice(&key[1..33]))
                            .ok_or(ArtifactError::Invalid("missing account preimage"))?;
                        encoded.extend_from_slice(address.as_slice());
                    } else if key[0] == key::STORAGE_ZONE {
                        let address = state
                            .address_preimage(B256::from_slice(&key[1..33]))
                            .ok_or(ArtifactError::Invalid("missing account preimage"))?;
                        let slot = state
                            .storage_preimage(address, key)
                            .ok_or(ArtifactError::Invalid("missing storage preimage"))?;
                        encoded.extend_from_slice(&slot.to_be_bytes::<32>());
                    }
                }
                Node::Branch { prefix, left, right, .. } => {
                    encoded.push(BRANCH);
                    encoded.extend_from_slice(
                        &u16::try_from(prefix.len())
                            .map_err(|_| ArtifactError::ResourceLimit)?
                            .to_be_bytes(),
                    );
                    let start = encoded.len();
                    encoded.resize(start + prefix.len().div_ceil(8), 0);
                    for (index, bit) in prefix.iter().enumerate() {
                        encoded[start + index / 8] |= u8::from(bit) << (7 - index % 8);
                    }
                    encoded.extend_from_slice(left.hash().as_slice());
                    encoded.extend_from_slice(right.hash().as_slice());
                }
            }
            batch.put(database_key(NODE, hash), encoded);
            if batch.size_in_bytes() >= WRITE_BATCH_BYTES {
                write(&self.db, batch)?;
                batch = WriteBatch::default();
            }
        }
        // All batches, including this root record, reach the WAL before a reference is published.
        batch.put(
            database_key(ROOT, state.root()),
            u64::try_from(state.tree().len())
                .map_err(|_| ArtifactError::ResourceLimit)?
                .to_be_bytes(),
        );
        write(&self.db, batch)
    }

    /// Authenticates native nodes and reconstructs the complete in-memory state.
    /// Hold `mutation` while loading. Missing/corrupt nodes or exhausted limits are errors.
    pub fn load<H: TreeHasher>(
        &self,
        root: B256,
        count: usize,
        limits: DecodeLimits,
    ) -> Result<StateTree<H>, NodeError> {
        if count > limits.max_leaves {
            return Err(ArtifactError::ResourceLimit.into());
        }
        let mut leaves = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        let mut slots = BTreeMap::new();
        let mut stack = Vec::new();
        if !root.is_zero() {
            stack.push((root, 0usize));
        }
        let mut visited = 0usize;
        while !stack.is_empty() {
            let frontier = stack.split_off(stack.len().saturating_sub(256));
            let keys: Vec<_> = frontier.iter().map(|(hash, _)| database_key(NODE, *hash)).collect();
            let cf = self.db.cf_handle("default").expect("default column family is open");
            let values = self.db.batched_multi_get_cf(cf, &keys, false);
            for ((hash, depth), value) in frontier.into_iter().zip(values) {
                visited = visited.checked_add(1).ok_or(ArtifactError::ResourceLimit)?;
                if visited > count.saturating_mul(2) || depth > STORAGE_KEY_LENGTH * 8 {
                    return Err(ArtifactError::ResourceLimit.into());
                }
                let value = value?.ok_or(ArtifactError::Invalid("missing PBT node"))?;
                match Self::decode_node::<H>(hash, &value)? {
                    StoredNode::Leaf { key, value, preimage } => {
                        if leaves.len() >= count || leaves.insert(key.clone(), value).is_some() {
                            return Err(
                                ArtifactError::Invalid("duplicate or excess PBT leaf").into()
                            );
                        }
                        if let Some(preimage) = preimage {
                            if key[0] == key::ACCOUNT_ZONE {
                                addresses.insert(
                                    B256::from_slice(&key[1..33]),
                                    Address::from_slice(&preimage),
                                );
                                if addresses.len() > limits.max_accounts {
                                    return Err(ArtifactError::ResourceLimit.into());
                                }
                            } else {
                                slots.insert(key, U256::from_be_slice(&preimage));
                            }
                        }
                    }
                    StoredNode::Branch { bits, left, right } => {
                        let depth =
                            depth.checked_add(bits + 1).ok_or(ArtifactError::ResourceLimit)?;
                        stack.push((right, depth));
                        stack.push((left, depth));
                    }
                }
            }
        }
        if leaves.len() != count {
            return Err(ArtifactError::Invalid("PBT leaf count mismatch").into());
        }
        Ok(restore_leaves::<H>(leaves, addresses, slots, root, limits)?)
    }

    fn node<H: TreeHasher>(&self, hash: B256) -> Result<StoredNode, NodeError> {
        let value = self
            .db
            .get_pinned(database_key(NODE, hash))?
            .ok_or(ArtifactError::Invalid("missing PBT node"))?;
        Self::decode_node::<H>(hash, &value)
    }

    fn decode_node<H: TreeHasher>(hash: B256, value: &[u8]) -> Result<StoredNode, NodeError> {
        if value.len() > MAX_NODE_BYTES {
            return Err(ArtifactError::ResourceLimit.into());
        }
        let mut reader = Reader(value);
        let tag = reader.take(1)?[0];
        let length = usize::from(u16::from_be_bytes(reader.array()?));
        let node = match tag {
            LEAF => {
                if !matches!(length, ACCOUNT_KEY_LENGTH | STORAGE_KEY_LENGTH) {
                    return Err(ArtifactError::Invalid("invalid PBT key length").into());
                }
                let key = reader.take(length)?.to_vec();
                if !matches!(
                    (key[0], length),
                    (key::ACCOUNT_ZONE | key::CODE_ZONE, ACCOUNT_KEY_LENGTH)
                        | (key::STORAGE_ZONE, STORAGE_KEY_LENGTH)
                ) {
                    return Err(ArtifactError::Invalid("invalid PBT key zone").into());
                }
                let value = B256::from_slice(reader.take(32)?);
                if value.is_zero() || leaf_hash::<H>(&key, value) != hash {
                    return Err(ArtifactError::Invalid("invalid PBT leaf hash").into());
                }
                let preimage = if key[0] == key::ACCOUNT_ZONE
                    && matches!(key[33], CODE_HASH_LEAF_KEY | DELEGATION_LEAF_KEY)
                {
                    Some(reader.take(20)?.to_vec())
                } else if key[0] == key::STORAGE_ZONE {
                    Some(reader.take(32)?.to_vec())
                } else {
                    None
                };
                StoredNode::Leaf { key, value, preimage }
            }
            BRANCH => {
                if length >= STORAGE_KEY_LENGTH * 8 {
                    return Err(ArtifactError::Invalid("invalid PBT branch length").into());
                }
                let packed = reader.take(length.div_ceil(8))?;
                if length % 8 != 0
                    && packed.last().is_some_and(|last| *last & ((1 << (8 - length % 8)) - 1) != 0)
                {
                    return Err(ArtifactError::Invalid("noncanonical PBT prefix padding").into());
                }
                let prefix: Vec<_> = (0..length)
                    .map(|index| packed[index / 8] & (1 << (7 - index % 8)) != 0)
                    .collect();
                let left = B256::from_slice(reader.take(32)?);
                let right = B256::from_slice(reader.take(32)?);
                if left.is_zero()
                    || right.is_zero()
                    || branch_hash::<H>(&prefix, left, right) != hash
                {
                    return Err(ArtifactError::Invalid("invalid PBT branch hash").into());
                }
                StoredNode::Branch { bits: length, left, right }
            }
            _ => return Err(ArtifactError::Invalid("invalid PBT node tag").into()),
        };
        if !reader.0.is_empty() {
            return Err(ArtifactError::Invalid("trailing PBT node bytes").into());
        }
        Ok(node)
    }

    /// Destructively unregisters a root after external references have been removed.
    /// Hold `mutation`; shared nodes are retained until `collect` runs.
    pub fn remove_root(&self, root: B256) -> Result<(), NodeError> {
        let mut batch = WriteBatch::default();
        batch.delete(database_key(ROOT, root));
        write(&self.db, batch)
    }

    /// Destructively reclaims nodes unreachable from every registered root.
    /// Hold `mutation` throughout collection. Interrupted marking deletes nothing.
    /// A corrupt retained node fails marking before the sweep begins.
    /// An interrupted sweep is completed before the next save can register a root.
    pub fn collect<H: TreeHasher>(&self) -> Result<(), NodeError> {
        self.collect_with::<H>(WRITE_BATCH_BYTES, || Ok(()))
    }

    fn collect_with<H: TreeHasher>(
        &self,
        sweep_batch_bytes: usize,
        mut after_sweep_batch: impl FnMut() -> Result<(), NodeError>,
    ) -> Result<(), NodeError> {
        self.delete_prefix(MARK)?;
        for root in self.hashes(ROOT) {
            let root = root?;
            let mut stack = Vec::new();
            if !root.is_zero() {
                stack.push((root, 0usize));
            }
            let mut batch = WriteBatch::default();
            while let Some((hash, depth)) = stack.pop() {
                if depth > STORAGE_KEY_LENGTH * 8 {
                    return Err(ArtifactError::Invalid("invalid PBT node depth").into());
                }
                if self.db.get_pinned(database_key(MARK, hash))?.is_some() {
                    continue;
                }
                if let StoredNode::Branch { bits, left, right } = self.node::<H>(hash)? {
                    stack.push((right, depth + bits + 1));
                    stack.push((left, depth + bits + 1));
                }
                batch.put(database_key(MARK, hash), []);
                if batch.size_in_bytes() >= WRITE_BATCH_BYTES {
                    write(&self.db, batch)?;
                    batch = WriteBatch::default();
                }
            }
            write(&self.db, batch)?;
        }
        // Persist this before the first deletion and keep it until every sweep batch
        // and marker cleanup has reached the WAL, including across process restarts.
        let mut batch = WriteBatch::default();
        batch.put(GC_PENDING, []);
        write(&self.db, batch)?;
        let mut batch = WriteBatch::default();
        for hash in self.hashes(NODE) {
            let hash = hash?;
            if self.db.get_pinned(database_key(MARK, hash))?.is_none() {
                batch.delete(database_key(NODE, hash));
            }
            if batch.size_in_bytes() >= sweep_batch_bytes {
                write(&self.db, batch)?;
                after_sweep_batch()?;
                batch = WriteBatch::default();
            }
        }
        write(&self.db, batch)?;
        self.delete_prefix(MARK)?;
        let mut batch = WriteBatch::default();
        batch.delete(GC_PENDING);
        write(&self.db, batch)
    }

    fn delete_prefix(&self, prefix: u8) -> Result<(), NodeError> {
        let mut batch = WriteBatch::default();
        for hash in self.hashes(prefix) {
            batch.delete(database_key(prefix, hash?));
            if batch.size_in_bytes() >= WRITE_BATCH_BYTES {
                write(&self.db, batch)?;
                batch = WriteBatch::default();
            }
        }
        write(&self.db, batch)
    }

    /// Reports whether any root is registered, returning errors for unreadable records.
    /// Hold `mutation` when this result participates in checkpoint publication.
    pub fn has_roots(&self) -> Result<bool, NodeError> {
        Ok(self.hashes(ROOT).next().transpose()?.is_some())
    }

    fn hashes(&self, prefix: u8) -> impl Iterator<Item = Result<B256, NodeError>> + '_ {
        // Key-only iteration avoids allocating corrupt, potentially oversized node values in GC.
        let mut iterator = self.db.raw_iterator();
        iterator.seek([prefix]);
        let mut finished = false;
        std::iter::from_fn(move || {
            if finished {
                return None;
            }
            let Some(key) = iterator.key() else {
                finished = true;
                return iterator.status().err().map(|error| Err(error.into()));
            };
            if key.first() != Some(&prefix) {
                finished = true;
                return None;
            }
            let hash = if key.len() == 33 {
                Ok(B256::from_slice(&key[1..]))
            } else {
                Err(ArtifactError::Invalid("invalid PBT database key").into())
            };
            iterator.next();
            Some(hash)
        })
    }
}

/// A native trie database, directory, or state-authentication operation failed.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The RocksDB operation failed.
    #[error(transparent)]
    Database(#[from] rocksdb::Error),
    /// Node content, metadata, state, or the requested resource budget is invalid.
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    /// The database directory could not be resolved.
    #[error("could not resolve PBT database directory {path:?}: {source}")]
    Io {
        /// Directory passed to the database opener.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// A panicking writer poisoned the process-local database registry.
    #[error("PBT native database lock poisoned")]
    Poisoned,
}

enum StoredNode {
    Leaf { key: Vec<u8>, value: B256, preimage: Option<Vec<u8>> },
    Branch { bits: usize, left: B256, right: B256 },
}

/// Version of the native RocksDB node/marker format, distinct from artifact encoding.
pub const STORE_VERSION: u32 = 3;
const NODE: u8 = b'n';
const ROOT: u8 = b'r';
const MARK: u8 = b'g';
// This metadata key deliberately does not share the node/root/mark hash prefixes.
const GC_PENDING: &[u8] = b"pending_gc";
const LEAF: u8 = 0;
const BRANCH: u8 = 1;
const WRITE_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODE_BYTES: usize = 3 + STORAGE_KEY_LENGTH + 64;

fn database_key(prefix: u8, hash: B256) -> [u8; 33] {
    let mut key = [prefix; 33];
    key[1..].copy_from_slice(hash.as_slice());
    key
}

fn write(db: &DB, batch: WriteBatch) -> Result<(), NodeError> {
    let mut options = WriteOptions::default();
    options.set_sync(true);
    Ok(db.write_opt(batch, &options)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountData, Blake3Hasher, migration::Artifacts};
    use alloy_primitives::Bytes;

    #[test]
    fn interrupted_sweep_recovers_before_resaving_orphan_subtrees() {
        for legacy in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let database = NodeDatabase::open(directory.path()).unwrap();
            let limits = DecodeLimits::default();
            let mut retained = StateTree::<Blake3Hasher>::new();
            retained
                .set_account(
                    Address::repeat_byte(2),
                    AccountData { nonce: 1, ..Default::default() },
                )
                .unwrap();
            let mut orphan = StateTree::<Blake3Hasher>::new();
            let address = Address::repeat_byte(1);
            orphan.set_account(address, AccountData { nonce: 1, ..Default::default() }).unwrap();
            for slot in 64..192 {
                orphan.set_storage(address, U256::from(slot), B256::repeat_byte(1)).unwrap();
            }
            // Keep the parent late in hash order, while the first sweep batch
            // durably deletes a child. This is the state left by an interrupted GC.
            for nonce in 1..1024 {
                orphan.set_account(address, AccountData { nonce, ..Default::default() }).unwrap();
                if orphan.root()[0] >= 0xf8 {
                    break;
                }
            }
            assert!(orphan.root()[0] >= 0xf8);
            {
                let _guard = database.mutation.lock().unwrap();
                database.save(&retained, limits).unwrap();
                database.save(&orphan, limits).unwrap();
                database.remove_root(orphan.root()).unwrap();
                let result = database.collect_with::<Blake3Hasher>(1, || {
                    Err(ArtifactError::Invalid("interrupted sweep").into())
                });
                assert!(result.is_err());
                assert!(database.db.get(GC_PENDING).unwrap().is_some());
                assert!(database.db.get(database_key(NODE, orphan.root())).unwrap().is_some());
                assert!(
                    database
                        .load::<Blake3Hasher>(orphan.root(), orphan.tree().len(), limits)
                        .is_err()
                );
                if legacy {
                    // Stores written by the previous implementation had no GC markers.
                    let mut batch = WriteBatch::default();
                    batch.delete(GC_PENDING);
                    batch.put(b"schema", 2u32.to_be_bytes());
                    write(&database.db, batch).unwrap();
                }
            }
            drop(database);
            let database = NodeDatabase::open(directory.path()).unwrap();
            let _guard = database.mutation.lock().unwrap();
            database.save(&orphan, limits).unwrap();
            assert!(database.db.get(GC_PENDING).unwrap().is_none());
            for state in [&retained, &orphan] {
                let restored = database
                    .load::<Blake3Hasher>(state.root(), state.tree().len(), limits)
                    .unwrap();
                assert_eq!(
                    Artifacts::from_state(&restored).unwrap(),
                    Artifacts::from_state(state).unwrap()
                );
            }
            let sequence = database.db.latest_sequence_number();
            database.save(&orphan, limits).unwrap();
            assert_eq!(database.db.latest_sequence_number() - sequence, 1);
        }
    }

    #[test]
    fn checkpoints_write_only_changed_paths_and_collect_unshared_nodes() {
        let directory = tempfile::tempdir().unwrap();
        let database = NodeDatabase::open(directory.path()).unwrap();
        assert!(Arc::ptr_eq(&database, &NodeDatabase::open(directory.path()).unwrap()));
        let limits = DecodeLimits::default();
        let mut state = StateTree::<Blake3Hasher>::new();
        state.set_account(Address::ZERO, AccountData { nonce: 1, ..Default::default() }).unwrap();
        for slot in 0..1024 {
            state.set_storage(Address::ZERO, U256::from(slot), B256::repeat_byte(1)).unwrap();
        }
        database.save(&state, limits).unwrap();
        let first_root = state.root();
        let first_count = node_count(&database);
        let sequence = database.db.latest_sequence_number();
        database.save(&state, limits).unwrap();
        assert_eq!(node_count(&database), first_count);
        assert_eq!(database.db.latest_sequence_number() - sequence, 1);
        let parent = state.clone();
        state.set_storage(Address::ZERO, U256::from(1023), B256::repeat_byte(2)).unwrap();
        let sequence = database.db.latest_sequence_number();
        database.save(&state, limits).unwrap();
        let added = node_count(&database) - first_count;
        assert!(added > 1 && added <= STORAGE_KEY_LENGTH * 8 + 1, "added {added} nodes");
        assert_eq!(
            database.db.latest_sequence_number() - sequence,
            u64::try_from(added + 1).unwrap()
        );
        assert_eq!(parent.storage(Address::ZERO, U256::from(1023)), B256::repeat_byte(1));
        assert_eq!(
            database.load::<Blake3Hasher>(first_root, parent.tree().len(), limits).unwrap().root(),
            first_root
        );
        database.remove_root(first_root).unwrap();
        database.collect::<Blake3Hasher>().unwrap();
        assert_eq!(node_count(&database), state.tree().len() * 2 - 1);
        assert_eq!(
            database.load::<Blake3Hasher>(state.root(), state.tree().len(), limits).unwrap().root(),
            state.root()
        );
        // Interrupted marking leaves hints that the next collection must rebuild.
        database.db.put(database_key(MARK, first_root), []).unwrap();
        database.collect::<Blake3Hasher>().unwrap();
        assert_eq!(node_count(&database), state.tree().len() * 2 - 1);
    }

    #[test]
    fn cold_load_verifies_code_delegations_preimages_and_node_hashes() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = StateTree::<Blake3Hasher>::new();
        for byte in 1..=3 {
            let mut code = if byte == 3 { vec![0xef, 1, 0] } else { vec![0x7f; 95] };
            if byte == 3 {
                code.extend_from_slice(Address::ZERO.as_slice());
            }
            let address = Address::repeat_byte(byte);
            state
                .set_account(
                    address,
                    AccountData { nonce: 1, code: Bytes::from(code), ..Default::default() },
                )
                .unwrap();
            state.set_storage(address, U256::from(63), B256::repeat_byte(5)).unwrap();
            state.set_storage(address, U256::MAX, B256::repeat_byte(6)).unwrap();
        }
        let limits = DecodeLimits::default();
        let database = NodeDatabase::open(directory.path()).unwrap();
        database.save(&state, limits).unwrap();
        drop(database);
        let database = NodeDatabase::open(directory.path()).unwrap();
        let restored =
            database.load::<Blake3Hasher>(state.root(), state.tree().len(), limits).unwrap();
        assert_eq!(
            Artifacts::from_state(&restored).unwrap(),
            Artifacts::from_state(&state).unwrap()
        );
        let (key, _) = state.tree().entries().find(|(key, _)| key[0] == key::STORAGE_ZONE).unwrap();
        let hash = leaf_hash::<Blake3Hasher>(key, state.tree().get(key).unwrap());
        let record = database_key(NODE, hash);
        let original = database.db.get(record).unwrap().unwrap();
        let mut corrupted = original.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        database.db.put(record, &corrupted).unwrap();
        assert!(matches!(
            database.load::<Blake3Hasher>(state.root(), state.tree().len(), limits),
            Err(NodeError::Artifact(ArtifactError::Invalid("invalid storage preimage")))
        ));
        database.db.put(record, original).unwrap();
        database.db.delete(record).unwrap();
        assert!(matches!(
            database.load::<Blake3Hasher>(state.root(), state.tree().len(), limits),
            Err(NodeError::Artifact(ArtifactError::Invalid("missing PBT node")))
        ));
    }

    #[test]
    fn schema_and_empty_account_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let database = NodeDatabase::open(directory.path()).unwrap();
        let mut state = StateTree::<Blake3Hasher>::new();
        state.set_account(Address::ZERO, AccountData::default()).unwrap();
        assert!(matches!(
            database.save(&state, DecodeLimits::default()),
            Err(NodeError::Artifact(ArtifactError::Invalid("empty anchor account")))
        ));
        database.db.put(b"schema", 0u32.to_be_bytes()).unwrap();
        drop(database);
        assert!(matches!(
            NodeDatabase::open(directory.path()),
            Err(NodeError::Artifact(ArtifactError::Invalid("unsupported PBT database version")))
        ));
    }

    fn node_count(database: &NodeDatabase) -> usize {
        database.hashes(NODE).map(Result::unwrap).count()
    }
}
