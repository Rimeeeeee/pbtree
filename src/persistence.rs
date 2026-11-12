//! Content-addressed trie nodes shared by durable fork checkpoints.

use crate::{
    Proof, ProofStep, ProofTerminal, StateTree, TreeHasher,
    hash::{branch_hash, leaf_hash},
    key::{self, ACCOUNT_KEY_LENGTH, CODE_HASH_LEAF_KEY, DELEGATION_LEAF_KEY, STORAGE_KEY_LENGTH},
    migration::{ArtifactError, DecodeLimits, Reader, restore_leaves},
    tree::{Node, bit, matches_prefix, validate_key},
};
use alloy_primitives::{Address, B256, U256};
use rocksdb::{DB, Options, WriteBatch, WriteOptions};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
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
    generation: AtomicU64,
    pins: Mutex<BTreeMap<B256, usize>>,
    collecting: AtomicBool,
    restoration: Mutex<()>,
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
        let database = Arc::new(Self {
            db,
            mutation: Mutex::default(),
            generation: AtomicU64::new(0),
            pins: Mutex::default(),
            collecting: AtomicBool::new(false),
            restoration: Mutex::default(),
        });
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
        let mut sweeping = self.db.get(GC_PENDING)?.is_some();
        if sweeping && !self.collecting.load(Ordering::Acquire) {
            self.collect::<H>()?;
            sweeping = false;
        }
        let mut stack: Vec<_> =
            state.tree().root.as_deref().map(|node| (node, false)).into_iter().collect();
        let mut batch = WriteBatch::default();
        while let Some((node, expanded)) = stack.pop() {
            let hash = node.hash();
            if !expanded
                && self.db.get_pinned(database_key(NODE, hash))?.is_some()
                && (!sweeping || self.db.get_pinned(database_key(MARK, hash))?.is_some())
            {
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
                        encoded[start + index / 8] |= u8::from(*bit) << (7 - index % 8);
                    }
                    encoded.extend_from_slice(left.hash().as_slice());
                    encoded.extend_from_slice(right.hash().as_slice());
                }
            }
            batch.put(database_key(NODE, hash), encoded);
            // During a cooperative sweep, publish only complete, marked subtrees.
            // Unmarked existing parents may already have lost children; rebuild
            // them from the caller's immutable state rather than trusting existence.
            if sweeping {
                batch.put(database_key(MARK, hash), []);
            }
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
    /// Hold `mutation` or a root lease while loading. Missing/corrupt nodes or exhausted limits are
    /// errors.
    pub fn load<H: TreeHasher>(
        &self,
        root: B256,
        count: usize,
        limits: DecodeLimits,
    ) -> Result<StateTree<H>, NodeError> {
        // Bound full-state reconstruction memory independently of checkpoint writes.
        // This lock never acquires `mutation`; callers may instead protect reads
        // with a root lease and leave writers free to publish unrelated states.
        let _restoration = self.restoration.lock().map_err(|_| NodeError::Poisoned)?;
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
                    StoredNode::Branch { prefix, left, right } => {
                        let depth = depth
                            .checked_add(prefix.len() + 1)
                            .ok_or(ArtifactError::ResourceLimit)?;
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
                StoredNode::Branch { prefix, left, right }
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
        self.remove_root_with(root, || {})
    }

    fn remove_root_with(&self, root: B256, after_write: impl FnOnce()) -> Result<(), NodeError> {
        if self.is_pinned(root)? {
            return Err(ArtifactError::Invalid("PBT root is pinned").into());
        }
        let mut batch = WriteBatch::default();
        batch.delete(database_key(ROOT, root));
        let result = write(&self.db, batch);
        after_write();
        // Readers can inspect registered roots without taking mutation. Publish the
        // generation after deletion so they cannot retain a root about to disappear.
        self.invalidate_caches();
        result
    }

    /// Destructively reclaims nodes unreachable from every registered root.
    /// Hold `mutation` throughout collection. Interrupted marking deletes nothing.
    /// A corrupt retained node fails marking before the sweep begins.
    /// An interrupted sweep is completed before the next save can register a root.
    pub fn collect<H: TreeHasher>(&self) -> Result<(), NodeError> {
        let _collection = self
            .begin_collection()
            .ok_or(ArtifactError::Invalid("PBT collection already running"))?;
        self.collect_with::<H>(WRITE_BATCH_BYTES, || Ok(()))
    }

    /// Reclaims unreachable nodes while allowing checkpoint writers to progress.
    ///
    /// Do not hold `mutation` when calling. Marking runs without it; a short write
    /// barrier marks roots published during marking. Sweeping takes the lock only
    /// for bounded batches and rechecks marks before deletion. Concurrent saves
    /// repair and mark reused subtrees before publishing their roots. Returns false
    /// if another collection is active. Unreferenced nodes allocated during a sweep
    /// may survive until the next pass. Failed sweeps remain recoverable on reopen.
    pub fn collect_cooperatively<H: TreeHasher>(&self) -> Result<bool, NodeError> {
        self.collect_cooperatively_with::<H>(GC_SWEEP_NODES, |_| Ok(()))
    }

    fn collect_cooperatively_with<H: TreeHasher>(
        &self,
        sweep_nodes: usize,
        mut checkpoint: impl FnMut(CollectionPhase) -> Result<(), NodeError>,
    ) -> Result<bool, NodeError> {
        let writer = self.mutation.lock().map_err(|_| NodeError::Poisoned)?;
        let Some(_collection) = self.begin_collection() else { return Ok(false) };
        if self.db.get(GC_PENDING)?.is_some() {
            self.collect_with::<H>(WRITE_BATCH_BYTES, || Ok(()))?;
        }
        drop(writer);
        // No sweeper is active yet and saves do not add marks before GC_PENDING.
        self.delete_prefix(MARK)?;
        let roots = {
            let _writer = self.mutation.lock().map_err(|_| NodeError::Poisoned)?;
            self.roots().collect::<Result<Vec<_>, _>>()?
        };
        for root in roots {
            self.mark_tree::<H>(root, GC_MARK_BYTES)?;
        }
        checkpoint(CollectionPhase::Marked)?;
        {
            let _writer = self.mutation.lock().map_err(|_| NodeError::Poisoned)?;
            // Root publication is serialized with this barrier. Existing marked
            // subtrees make catching up to newly published roots incremental.
            for root in self.roots() {
                self.mark_tree::<H>(root?, GC_MARK_BYTES)?;
            }
            let mut batch = WriteBatch::default();
            batch.put(GC_PENDING, []);
            write(&self.db, batch)?;
        }
        let mut candidates = Vec::with_capacity(sweep_nodes);
        for hash in self.hashes(NODE) {
            candidates.push(hash?);
            if candidates.len() >= sweep_nodes {
                self.sweep_cooperative_batch(&candidates)?;
                candidates.clear();
                checkpoint(CollectionPhase::Swept)?;
                std::thread::yield_now();
            }
        }
        if !candidates.is_empty() {
            self.sweep_cooperative_batch(&candidates)?;
            checkpoint(CollectionPhase::Swept)?;
        }
        {
            let _writer = self.mutation.lock().map_err(|_| NodeError::Poisoned)?;
            // Every remaining parent now has complete children, including nodes
            // published during the sweep. Disable the save barrier before clearing
            // marks, so concurrent saves cannot mistake cleared marks for holes.
            let mut batch = WriteBatch::default();
            batch.delete(GC_PENDING);
            write(&self.db, batch)?;
        }
        self.delete_prefix(MARK)?;
        Ok(true)
    }

    fn sweep_cooperative_batch(&self, candidates: &[B256]) -> Result<(), NodeError> {
        let _writer = self.mutation.lock().map_err(|_| NodeError::Poisoned)?;
        let mut batch = WriteBatch::default();
        for hash in candidates {
            // A save may have repaired and marked this hash since iteration began.
            if self.db.get_pinned(database_key(MARK, *hash))?.is_none() {
                batch.delete(database_key(NODE, *hash));
            }
        }
        write(&self.db, batch)
    }

    fn begin_collection(&self) -> Option<CollectionGuard<'_>> {
        if self
            .collecting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        Some(CollectionGuard(&self.collecting))
    }

    fn collect_with<H: TreeHasher>(
        &self,
        sweep_batch_bytes: usize,
        mut after_sweep_batch: impl FnMut() -> Result<(), NodeError>,
    ) -> Result<(), NodeError> {
        self.delete_prefix(MARK)?;
        for root in self.hashes(ROOT) {
            self.mark_tree::<H>(root?, WRITE_BATCH_BYTES)?;
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

    fn mark_tree<H: TreeHasher>(&self, root: B256, batch_bytes: usize) -> Result<(), NodeError> {
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
            if let StoredNode::Branch { prefix, left, right } = self.node::<H>(hash)? {
                stack.push((right, depth + prefix.len() + 1));
                stack.push((left, depth + prefix.len() + 1));
            }
            batch.put(database_key(MARK, hash), []);
            if batch.size_in_bytes() >= batch_bytes {
                write(&self.db, batch)?;
                batch = WriteBatch::default();
            }
        }
        write(&self.db, batch)?;
        Ok(())
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

    /// Returns registered roots without materializing their states. Hold `mutation`
    /// when using the iterator to select roots for destructive collection.
    pub fn roots(&self) -> impl Iterator<Item = Result<B256, NodeError>> + '_ {
        self.hashes(ROOT)
    }

    /// Returns the process-local invalidation generation shared by all database handles.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Invalidates client caches after removing external references or roots.
    /// Hold `mutation` until invalidation completes; readers missing their cache must take it.
    pub fn invalidate_caches(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Pins registered roots while a client verifies or commits an external reference.
    /// Hold `mutation` while acquiring the lease, then release it before taking another
    /// database's writer lock. Removal fails until every lease for a root is dropped.
    pub fn pin_roots(self: &Arc<Self>, roots: &[B256]) -> Result<RootLease, NodeError> {
        let mut pins = self.pins.lock().map_err(|_| NodeError::Poisoned)?;
        for root in roots {
            self.leaf_count(*root)?.ok_or(ArtifactError::Invalid("unregistered PBT root"))?;
            if pins.get(root) == Some(&usize::MAX) {
                return Err(ArtifactError::ResourceLimit.into());
            }
        }
        let mut roots = roots.to_vec();
        roots.sort_unstable();
        roots.dedup();
        for root in &roots {
            *pins.entry(*root).or_default() += 1;
        }
        Ok(RootLease { database: self.clone(), roots })
    }

    /// Reports whether a process-local lease protects a root. Hold `mutation` when
    /// using this result to remove external references or checkpoint marker files.
    pub fn is_pinned(&self, root: B256) -> Result<bool, NodeError> {
        Ok(self.pins.lock().map_err(|_| NodeError::Poisoned)?.contains_key(&root))
    }

    /// Reads and authenticates only the nodes on a key's search path.
    ///
    /// Hold `mutation` or a root lease to prevent removal or collection. The root must
    /// be registered. Unlike `load`, this does not reconstruct account/code/storage
    /// metadata or allocate memory proportional to the complete state.
    pub fn proof<H: TreeHasher>(&self, root: B256, key: &[u8]) -> Result<Proof, NodeError> {
        validate_key(key).map_err(|_| ArtifactError::Invalid("invalid PBT proof key"))?;
        self.leaf_count(root)?.ok_or(ArtifactError::Invalid("unregistered PBT root"))?;
        let mut steps = Vec::new();
        let mut hash = root;
        let mut depth = 0;
        let terminal = loop {
            if hash.is_zero() {
                break ProofTerminal::Empty;
            }
            if depth > STORAGE_KEY_LENGTH * 8 {
                return Err(ArtifactError::Invalid("invalid PBT node depth").into());
            }
            match self.node::<H>(hash)? {
                StoredNode::Leaf { key, value, .. } => break ProofTerminal::Leaf { key, value },
                StoredNode::Branch { prefix, left, right } => {
                    if !matches_prefix(key, depth, &prefix)
                        || bit(key, depth + prefix.len()).is_none()
                    {
                        break ProofTerminal::Branch { prefix, left, right };
                    }
                    depth += prefix.len();
                    let rightward =
                        bit(key, depth).ok_or(ArtifactError::Invalid("invalid PBT proof path"))?;
                    depth += 1;
                    hash = if rightward { right } else { left };
                    steps.push(ProofStep {
                        prefix,
                        right: rightward,
                        sibling: if rightward { left } else { right },
                    });
                }
            }
        };
        let proof = Proof { steps, terminal };
        proof
            .verify::<H>(root, key)
            .map_err(|_| ArtifactError::Invalid("invalid PBT proof path"))?;
        Ok(proof)
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

/// Keeps registered roots and their native nodes alive without holding a database
/// mutation lock. Dropping the lease releases pins; it does not unregister roots.
#[derive(Debug)]
#[must_use = "keep the lease alive until external references are committed"]
pub struct RootLease {
    database: Arc<NodeDatabase>,
    roots: Vec<B256>,
}

#[derive(Debug)]
struct CollectionGuard<'a>(&'a AtomicBool);

impl Drop for CollectionGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug)]
enum CollectionPhase {
    Marked,
    Swept,
}

impl Drop for RootLease {
    fn drop(&mut self) {
        // Release pins even after a client panic; drop cannot report a poisoned lock.
        let mut pins = self.database.pins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for root in &self.roots {
            if let Some(count) = pins.get_mut(root) {
                *count -= 1;
                if *count == 0 {
                    pins.remove(root);
                }
            }
        }
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
    /// A panicking native operation poisoned a process-local lock.
    #[error("PBT native database lock poisoned")]
    Poisoned,
}

enum StoredNode {
    Leaf { key: Vec<u8>, value: B256, preimage: Option<Vec<u8>> },
    Branch { prefix: Vec<bool>, left: B256, right: B256 },
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
const GC_MARK_BYTES: usize = 1024 * 1024;
const GC_SWEEP_NODES: usize = 1024;
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

    #[test]
    fn native_proofs_match_memory_without_loading_unrelated_subtrees() {
        let directory = tempfile::tempdir().unwrap();
        let database = NodeDatabase::open(directory.path()).unwrap();
        let mut state = StateTree::<Blake3Hasher>::new();
        for byte in 1..=8 {
            let address = Address::repeat_byte(byte);
            state.set_account(address, AccountData { nonce: 1, ..Default::default() }).unwrap();
            state.set_storage(address, U256::MAX, B256::repeat_byte(byte)).unwrap();
        }
        database.save(&state, DecodeLimits::native()).unwrap();
        for (key, value) in state.tree().entries() {
            let proof = database.proof::<Blake3Hasher>(state.root(), key).unwrap();
            assert_eq!(proof, state.tree().proof(key));
            assert_eq!(proof.verify::<Blake3Hasher>(state.root(), key), Ok(Some(value)));
        }
        for key in [vec![0], key::storage_key::<Blake3Hasher>(Address::ZERO, U256::MAX)] {
            assert_eq!(
                database.proof::<Blake3Hasher>(state.root(), &key).unwrap(),
                state.tree().proof(&key)
            );
        }
        // A path-local proof must not fetch the sibling subtree.
        let StoredNode::Branch { left, right, .. } =
            database.node::<Blake3Hasher>(state.root()).unwrap()
        else {
            panic!("expected branch")
        };
        let (key, _) = state.tree().entries().next().unwrap();
        let proof = state.tree().proof(key);
        let sibling = proof.steps[0].sibling;
        assert!(sibling == left || sibling == right);
        database.db.delete(database_key(NODE, sibling)).unwrap();
        assert_eq!(database.proof::<Blake3Hasher>(state.root(), key).unwrap(), proof);
        assert!(matches!(
            database.proof::<Blake3Hasher>(state.root(), &[]),
            Err(NodeError::Artifact(ArtifactError::Invalid("invalid PBT proof key")))
        ));
        assert!(matches!(
            database.proof::<Blake3Hasher>(B256::repeat_byte(0xff), key),
            Err(NodeError::Artifact(ArtifactError::Invalid("unregistered PBT root")))
        ));
        let empty = StateTree::<Blake3Hasher>::new();
        database.save(&empty, DecodeLimits::native()).unwrap();
        assert_eq!(
            database.proof::<Blake3Hasher>(empty.root(), key).unwrap().terminal,
            ProofTerminal::Empty
        );
    }

    #[test]
    fn root_removal_invalidates_every_handle_and_native_budgets_are_independent() {
        let directory = tempfile::tempdir().unwrap();
        let database = NodeDatabase::open(directory.path()).unwrap();
        let other = NodeDatabase::open(directory.path()).unwrap();
        let mut state = StateTree::<Blake3Hasher>::new();
        state.set_account(Address::ZERO, AccountData { nonce: 1, ..Default::default() }).unwrap();
        let tight = DecodeLimits { max_accounts: 0, ..Default::default() };
        assert!(matches!(
            database.save(&state, tight),
            Err(NodeError::Artifact(ArtifactError::ResourceLimit))
        ));
        database.save(&state, DecodeLimits::native()).unwrap();
        assert_eq!(database.roots().collect::<Result<Vec<_>, _>>().unwrap(), vec![state.root()]);
        let generation = other.generation();
        database
            .remove_root_with(state.root(), || {
                // A cache reader must not consume the next generation while the
                // root is still registered and keep that entry after removal.
                assert_eq!(other.generation(), generation);
                assert_eq!(other.leaf_count(state.root()).unwrap(), None);
            })
            .unwrap();
        assert_ne!(other.generation(), generation);
        assert_eq!(other.leaf_count(state.root()).unwrap(), None);
        assert_eq!(
            database
                .load::<Blake3Hasher>(state.root(), state.tree().len(), DecodeLimits::native())
                .unwrap()
                .root(),
            state.root()
        );
    }

    #[test]
    fn leases_protect_roots_across_handles_without_holding_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let database = NodeDatabase::open(directory.path()).unwrap();
        let other = NodeDatabase::open(directory.path()).unwrap();
        let mut state = StateTree::<Blake3Hasher>::new();
        state.set_account(Address::ZERO, AccountData { nonce: 1, ..Default::default() }).unwrap();
        let (first, second) = {
            let _mutation = database.mutation.lock().unwrap();
            database.save(&state, DecodeLimits::native()).unwrap();
            assert!(matches!(
                database.pin_roots(&[state.root(), B256::repeat_byte(9)]),
                Err(NodeError::Artifact(ArtifactError::Invalid("unregistered PBT root")))
            ));
            assert!(!database.is_pinned(state.root()).unwrap());
            (
                database.pin_roots(&[state.root(), state.root()]).unwrap(),
                other.pin_roots(&[state.root()]).unwrap(),
            )
        };
        assert!(other.mutation.try_lock().is_ok());
        assert!(matches!(
            other.remove_root(state.root()),
            Err(NodeError::Artifact(ArtifactError::Invalid("PBT root is pinned")))
        ));
        other.collect::<Blake3Hasher>().unwrap();
        let (key, _) = state.tree().entries().next().unwrap();
        assert_eq!(
            other.proof::<Blake3Hasher>(state.root(), key).unwrap(),
            state.tree().proof(key)
        );
        drop(first);
        assert!(other.is_pinned(state.root()).unwrap());
        drop(second);
        assert!(!other.is_pinned(state.root()).unwrap());
        other.remove_root(state.root()).unwrap();
    }

    #[test]
    fn cooperative_sweeps_preserve_new_roots_and_repair_partly_swept_subtrees() {
        for interrupt in [false, true] {
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
            let address = Address::repeat_byte(1);
            let mut orphan = StateTree::<Blake3Hasher>::new();
            orphan.set_account(address, AccountData { nonce: 1, ..Default::default() }).unwrap();
            for slot in 64..192 {
                orphan.set_storage(address, U256::from(slot), B256::repeat_byte(1)).unwrap();
            }
            for nonce in 1..1024 {
                orphan.set_account(address, AccountData { nonce, ..Default::default() }).unwrap();
                if orphan.root()[0] >= 0xf8 {
                    break;
                }
            }
            assert!(orphan.root()[0] >= 0xf8);
            {
                let _writer = database.mutation.lock().unwrap();
                database.save(&retained, limits).unwrap();
                database.save(&orphan, limits).unwrap();
                database.remove_root(orphan.root()).unwrap();
            }
            let mut published_during_marking = retained.clone();
            published_during_marking
                .set_account(
                    Address::repeat_byte(3),
                    AccountData { nonce: 1, ..Default::default() },
                )
                .unwrap();
            let mut repaired = false;
            let result = database.collect_cooperatively_with::<Blake3Hasher>(1, |phase| {
                assert!(!database.collect_cooperatively::<Blake3Hasher>()?);
                match phase {
                    CollectionPhase::Marked => {
                        let _writer = database.mutation.try_lock().unwrap();
                        database.save(&published_during_marking, limits)?;
                    }
                    CollectionPhase::Swept
                        if !repaired
                            && matches!(
                                database.load::<Blake3Hasher>(
                                    orphan.root(),
                                    orphan.tree().len(),
                                    limits
                                ),
                                Err(NodeError::Artifact(ArtifactError::Invalid(
                                    "missing PBT node"
                                )))
                            ) =>
                    {
                        // A parent still exists while one of its children is already gone.
                        assert!(database.db.get(database_key(NODE, orphan.root()))?.is_some());
                        let _writer = database.mutation.try_lock().unwrap();
                        database.save(&orphan, limits)?;
                        repaired = true;
                        if interrupt {
                            return Err(
                                ArtifactError::Invalid("interrupted cooperative sweep").into()
                            );
                        }
                    }
                    _ => {}
                }
                Ok(())
            });
            assert!(repaired);
            if interrupt {
                assert!(matches!(
                    result,
                    Err(NodeError::Artifact(ArtifactError::Invalid(
                        "interrupted cooperative sweep"
                    )))
                ));
                assert!(database.db.get(GC_PENDING).unwrap().is_some());
            } else {
                assert!(result.unwrap());
            }
            assert!(!database.collecting.load(Ordering::Acquire));
            let _writer = database.mutation.lock().unwrap();
            // This save must recover a failed sweep before trusting existing parents.
            database.save(&retained, limits).unwrap();
            assert!(database.db.get(GC_PENDING).unwrap().is_none());
            for state in [&retained, &orphan, &published_during_marking] {
                let restored = database
                    .load::<Blake3Hasher>(state.root(), state.tree().len(), limits)
                    .unwrap();
                assert_eq!(
                    Artifacts::from_state(&restored).unwrap(),
                    Artifacts::from_state(state).unwrap()
                );
            }
        }
    }

    fn node_count(database: &NodeDatabase) -> usize {
        database.hashes(NODE).map(Result::unwrap).count()
    }
}
