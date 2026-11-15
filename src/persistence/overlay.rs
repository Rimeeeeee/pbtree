//! Authenticated checkpoint reads and copy-on-write state changes.

use super::{
    ArtifactError, DecodeLimits, GC_PENDING, MARK, NODE, NodeDatabase, NodeError, ROOT, RootLease,
    StoredNode, WRITE_BATCH_BYTES, database_key, write,
};
use crate::{
    StateError, StateTree, TreeHasher,
    hash::{branch_hash, leaf_hash},
    key::{self, BASIC_DATA_LEAF_KEY, CODE_HASH_LEAF_KEY, DELEGATION_LEAF_KEY, encode_basic_data},
    tree::{bit, matches_prefix},
};
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use rocksdb::WriteBatch;
use std::{
    array,
    marker::PhantomData,
    sync::{Arc, Mutex, atomic::Ordering},
};

const SHARDS: usize = 16;
const MAX_DEPTH: usize = key::STORAGE_KEY_LENGTH * 8;

/// Fixed-capacity cache of authenticated native nodes. Branches store only child hashes.
#[derive(Debug)]
pub struct SubtreeCache<H> {
    database: Arc<NodeDatabase>,
    shards: [Mutex<Vec<Option<Arc<Node>>>>; SHARDS],
    hasher: PhantomData<H>,
}

impl<H: TreeHasher> SubtreeCache<H> {
    /// Creates a cache with at most the given number of entries. Zero disables caching.
    pub fn new(database: Arc<NodeDatabase>, entries: usize) -> Self {
        Self {
            database,
            shards: array::from_fn(|shard| {
                Mutex::new(vec![None; entries / SHARDS + usize::from(shard < entries % SHARDS)])
            }),
            hasher: PhantomData,
        }
    }

    /// Pins a registered root for path reads. Hold the database mutation guard while
    /// acquiring it, and authenticate the root against a trusted header or reference.
    pub fn checkpoint(self: &Arc<Self>, root: B256) -> Result<DiskCheckpoint<H>, NodeError> {
        let leaves = self
            .database
            .leaf_count(root)?
            .ok_or(ArtifactError::Invalid("unregistered PBT root"))?;
        if root.is_zero() != (leaves == 0) {
            return Err(invalid_path());
        }
        let lease = self.database.pin_roots(&[root])?;
        Ok(DiskCheckpoint { root, leaves, cache: self.clone(), lease: Arc::new(lease) })
    }

    fn read(&self, hash: B256) -> Result<Arc<Node>, NodeError> {
        let index = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) as usize;
        let shard = &self.shards[index % SHARDS];
        {
            let slots = shard.lock().map_err(|_| NodeError::Poisoned)?;
            if !slots.is_empty()
                && let Some(node) = &slots[index / SHARDS % slots.len()]
                && node.hash == hash
            {
                return Ok(node.clone());
            }
        }
        let kind = match self.database.node::<H>(hash)? {
            StoredNode::Leaf { key, value, preimage } => Kind::Leaf { key, value, preimage },
            StoredNode::Branch { prefix, left, right } => {
                Kind::Branch { prefix, left: Link::Disk(left), right: Link::Disk(right) }
            }
        };
        let node = Arc::new(Node { hash, kind });
        let mut slots = shard.lock().map_err(|_| NodeError::Poisoned)?;
        if !slots.is_empty() {
            let slot = index / SHARDS % slots.len();
            slots[slot] = Some(node.clone());
        }
        Ok(node)
    }
}

/// A registered checkpoint with a lease protecting its unchanged subtrees.
#[derive(Clone, Debug)]
pub struct DiskCheckpoint<H> {
    root: B256,
    leaves: usize,
    cache: Arc<SubtreeCache<H>>,
    lease: Arc<RootLease>,
}

impl<H: TreeHasher> DiskCheckpoint<H> {
    /// Opens an independent overlay over the checkpoint.
    pub fn overlay(&self) -> StateOverlay<H> {
        StateOverlay { parent: self.clone(), root: Link::Disk(self.root), leaves: self.leaves }
    }
    /// Fully authenticates and materializes the parent for work needing global indexes.
    pub fn restore(&self, limits: DecodeLimits) -> Result<StateTree<H>, NodeError> {
        self.lease.database.load(self.root, self.leaves, limits)
    }
}

/// The account fields from authoritative execution output.
#[derive(Debug)]
pub struct AccountUpdate {
    /// The executed account nonce.
    pub nonce: u64,
    /// The executed account balance.
    pub balance: U256,
    /// The executed code action.
    pub code: CodeUpdate,
}

/// Code supplied by the executor, or the hash of already committed code.
#[derive(Debug)]
pub enum CodeUpdate {
    /// The executor supplied the complete code bytes. Empty bytes clear delegation.
    Bytes(Bytes),
    /// Keep the prior code when its authenticated header matches this Keccak hash.
    Retain(B256),
}

/// A private state update holding changed paths and references to a pinned checkpoint.
#[derive(Debug)]
pub struct StateOverlay<H> {
    parent: DiskCheckpoint<H>,
    root: Link,
    leaves: usize,
}

impl<H: TreeHasher> StateOverlay<H> {
    /// Returns the current PBT commitment.
    pub fn root(&self) -> B256 {
        self.root.hash()
    }

    /// Returns the current leaf count.
    pub const fn leaf_count(&self) -> usize {
        self.leaves
    }

    /// Applies an executed account and its final slot values atomically. Wiping
    /// storage clears prior slots before applying replacements. Removing or replacing
    /// previously committed contract code requires the full state's ownership index.
    pub fn update_account(
        &mut self,
        address: Address,
        account: Option<AccountUpdate>,
        wipe_storage: bool,
        storage: impl IntoIterator<Item = (U256, B256)>,
    ) -> Result<(), OverlayError> {
        let position = key::account_hash::<H>(address);
        let (old_basic, old_code) = self.header(position)?;
        let clearing_delegation = matches!(&account, Some(AccountUpdate { code: CodeUpdate::Bytes(bytes), .. }) if key::delegation_target(bytes) == Some(Address::ZERO));
        let old_size = u32::from_be_bytes(old_basic[4..8].try_into().map_err(|_| invalid_path())?);
        let (next_code, basic, code_bytes) = if let Some(account) = account {
            let (code, size, bytes) = match account.code {
                CodeUpdate::Retain(hash) => {
                    if old_code.keccak() != Some(hash) {
                        return Err(OverlayError::MissingCode(hash));
                    }
                    (old_code, old_size, None)
                }
                CodeUpdate::Bytes(bytes) => {
                    let target = key::delegation_target(&bytes);
                    if bytes.is_empty() || target == Some(Address::ZERO) {
                        (HeaderCode::Empty, 0, None)
                    } else {
                        let size =
                            u32::try_from(bytes.len()).map_err(|_| StateError::CodeSizeOverflow)?;
                        if let Some(target) = target {
                            (HeaderCode::Delegation(target), size, None)
                        } else {
                            (HeaderCode::Contract(keccak256(&bytes)), size, Some(bytes))
                        }
                    }
                }
            };
            let basic = encode_basic_data(size, account.nonce, account.balance)?;
            (if basic.is_zero() { HeaderCode::Absent } else { code }, basic, bytes)
        } else {
            (HeaderCode::Absent, B256::ZERO, None)
        };
        if matches!(old_code, HeaderCode::Contract(_)) && old_code != next_code {
            return Err(OverlayError::NeedsFullState);
        }
        let mut next =
            Self { parent: self.parent.clone(), root: self.root.clone(), leaves: self.leaves };
        if (wipe_storage || next_code == HeaderCode::Absent) && old_code != HeaderCode::Absent {
            // Inline storage occupies the 01xxxxxx range of the header subindex.
            let inline = key::header_key_from_hash(position, key::HEADER_STORAGE_OFFSET);
            next.clear_prefix(&inline, inline.len() * 8 - 6)?;
            let mut overflow = inline;
            overflow[0] = key::STORAGE_ZONE;
            next.clear_prefix(&overflow, 33 * 8)?;
        }
        if basic != old_basic {
            next.put(key::header_key_from_hash(position, BASIC_DATA_LEAF_KEY).into(), basic, None)?;
        }
        if next_code != old_code {
            if let Some((field, _)) = old_code.leaf() {
                next.put(key::header_key_from_hash(position, field).into(), B256::ZERO, None)?;
            }
            if let Some((field, value)) = next_code.leaf() {
                next.put(
                    key::header_key_from_hash(position, field).into(),
                    value,
                    Some(address.to_vec()),
                )?;
            }
            if let (HeaderCode::Contract(hash), Some(bytes)) = (next_code, code_bytes) {
                let chunks = key::chunkify_code(&bytes);
                for (key, value) in key::code_keys::<H>(hash, chunks.len()).zip(chunks) {
                    if !value.is_zero() {
                        next.put(key.into(), value, None)?;
                    }
                }
            }
        }
        if next_code != HeaderCode::Absent {
            for (slot, value) in storage {
                let key = key::storage_key::<H>(address, slot);
                let preimage = (key[0] == key::STORAGE_ZONE && !value.is_zero())
                    .then(|| slot.to_be_bytes::<32>().to_vec());
                next.put(key, value, preimage)?;
            }
        } else if clearing_delegation && storage.into_iter().any(|(_, value)| !value.is_zero()) {
            return Err(StateError::MissingAccount(address).into());
        }
        *self = next;
        Ok(())
    }

    /// Writes changed nodes and syncs their root with the original database. Hold its
    /// database mutation guard through this call and publication of the external reference.
    pub fn save(&self, database: &NodeDatabase, limits: DecodeLimits) -> Result<(), OverlayError> {
        if !std::ptr::eq(database, self.parent.lease.database.as_ref()) {
            return Err(OverlayError::WrongDatabase);
        }
        if self.leaves > limits.max_leaves {
            return Err(NodeError::from(ArtifactError::ResourceLimit).into());
        }
        if limits.max_accounts != usize::MAX
            || limits.max_code_size != usize::MAX
            || limits.max_total_code_size != usize::MAX
        {
            return Err(OverlayError::NeedsFullState);
        }
        let mut sweeping = database.db.get(GC_PENDING).map_err(NodeError::from)?.is_some();
        if sweeping && !database.collecting.load(Ordering::Acquire) {
            database.collect::<H>()?;
            sweeping = false;
        }
        let mut batch = WriteBatch::default();
        let mut stack = vec![(&self.root, false)];
        while let Some((link, expanded)) = stack.pop() {
            let Link::Changed(node) = link else { continue };
            if !expanded && let Kind::Branch { left, right, .. } = &node.kind {
                stack.push((link, true));
                stack.push((right, false));
                stack.push((left, false));
                continue;
            }
            // A partially swept orphan can have the same hash. Rewrite changed nodes.
            // Disk children remain reachable from the pinned parent.
            batch.put(database_key(NODE, node.hash), node.encode());
            if sweeping {
                batch.put(database_key(MARK, node.hash), []);
            }
            if batch.size_in_bytes() >= WRITE_BATCH_BYTES {
                write(&database.db, batch)?;
                batch = WriteBatch::default();
            }
        }
        let count = u64::try_from(self.leaves)
            .map_err(|_| NodeError::from(ArtifactError::ResourceLimit))?;
        batch.put(database_key(ROOT, self.root()), count.to_be_bytes());
        write(&database.db, batch)?;
        Ok(())
    }

    fn header(&self, position: B256) -> Result<(B256, HeaderCode), NodeError> {
        let first = key::header_key_from_hash(position, BASIC_DATA_LEAF_KEY);
        // The three account fields share a path through the first six subindex bits.
        let Some((node, depth)) = self.locate_prefix(&first, first.len() * 8 - 2, |_, _| {})?
        else {
            return Ok((B256::ZERO, HeaderCode::Absent));
        };
        let basic = self.get_from(node.clone(), depth, &first)?.unwrap_or_default();
        let code = self.get_from(
            node.clone(),
            depth,
            &key::header_key_from_hash(position, CODE_HASH_LEAF_KEY),
        )?;
        let delegation =
            self.get_from(node, depth, &key::header_key_from_hash(position, DELEGATION_LEAF_KEY))?;
        let size = &basic[4..8];
        let code = match (code, delegation) {
            (None, None) if basic.is_zero() => HeaderCode::Absent,
            (Some(hash), None) if hash == KECCAK256_EMPTY && size == [0; 4] => HeaderCode::Empty,
            (Some(hash), None) if size != [0; 4] => HeaderCode::Contract(hash),
            (None, Some(value))
                if size == (key::DELEGATION_SIZE as u32).to_be_bytes()
                    && value[key::DELEGATION_SIZE..].iter().all(|b| *b == 0) =>
            {
                let target = key::delegation_target(&value[..key::DELEGATION_SIZE])
                    .filter(|target| !target.is_zero())
                    .ok_or_else(invalid_path)?;
                HeaderCode::Delegation(target)
            }
            _ => return Err(invalid_path()),
        };
        Ok((basic, code))
    }

    fn get_from(
        &self,
        mut link: Link,
        mut depth: usize,
        key: &[u8],
    ) -> Result<Option<B256>, NodeError> {
        while let Some(node) = link.read(&self.parent.cache)? {
            match &node.kind {
                Kind::Leaf { key: existing, value, .. } => {
                    if !same_prefix(existing, key, depth) {
                        return Err(invalid_path());
                    }
                    return Ok((existing == key).then_some(*value));
                }
                Kind::Branch { prefix, left, right } => {
                    if depth + prefix.len() >= MAX_DEPTH {
                        return Err(invalid_path());
                    }
                    if !matches_prefix(key, depth, prefix) {
                        return Ok(None);
                    }
                    depth += prefix.len();
                    let Some(direction) = bit(key, depth) else { return Ok(None) };
                    link = if direction { right } else { left }.clone();
                    depth += 1;
                }
            }
        }
        Ok(None)
    }

    fn locate_prefix(
        &self,
        key: &[u8],
        bits: usize,
        mut visit: impl FnMut(Arc<Node>, bool),
    ) -> Result<Option<(Link, usize)>, NodeError> {
        let mut link = self.root.clone();
        let mut depth = 0;
        loop {
            if depth >= bits {
                break;
            }
            let Some(node) = link.read(&self.parent.cache)? else { return Ok(None) };
            match &node.kind {
                Kind::Leaf { key: existing, .. } => {
                    if !same_prefix(existing, key, depth) {
                        return Err(invalid_path());
                    }
                    if !same_prefix(existing, key, bits) {
                        return Ok(None);
                    }
                    break;
                }
                Kind::Branch { prefix, left, right } => {
                    if depth + prefix.len() >= MAX_DEPTH {
                        return Err(invalid_path());
                    }
                    let remaining = bits - depth;
                    if !matches_prefix(key, depth, &prefix[..prefix.len().min(remaining)]) {
                        return Ok(None);
                    }
                    if prefix.len() >= remaining {
                        break;
                    }
                    let direction = bit(key, depth + prefix.len()).ok_or_else(invalid_path)?;
                    depth += prefix.len() + 1;
                    link = if direction { right } else { left }.clone();
                    visit(node, direction);
                }
            }
        }
        Ok(Some((link, depth)))
    }

    fn clear_prefix(&mut self, key: &[u8], bits: usize) -> Result<(), NodeError> {
        let mut path = Vec::new();
        let Some((link, depth)) = self.locate_prefix(key, bits, |node, direction| {
            path.push((node, direction));
        })?
        else {
            return Ok(());
        };
        // Existing checkpoints store a root count only. Count the removed subtree
        // without visiting its siblings or materializing storage metadata.
        let mut removed = 0usize;
        let mut pending = vec![(link, depth)];
        while let Some((link, depth)) = pending.pop() {
            let node = link.read(&self.parent.cache)?.ok_or_else(invalid_path)?;
            match &node.kind {
                Kind::Leaf { key: existing, .. }
                    if same_prefix(existing, key, bits) && depth <= existing.len() * 8 =>
                {
                    removed = removed
                        .checked_add(1)
                        .filter(|n| *n <= self.leaves)
                        .ok_or_else(invalid_path)?;
                }
                Kind::Branch { prefix, left, right } if depth + prefix.len() < MAX_DEPTH => {
                    let depth = depth + prefix.len() + 1;
                    pending.push((right.clone(), depth));
                    pending.push((left.clone(), depth));
                }
                _ => return Err(invalid_path()),
            }
        }
        self.root = self.rebuild(Link::Disk(B256::ZERO), path)?;
        self.leaves -= removed;
        Ok(())
    }

    fn put(
        &mut self,
        key: Vec<u8>,
        value: B256,
        preimage: Option<Vec<u8>>,
    ) -> Result<(), NodeError> {
        let mut link = self.root.clone();
        let mut depth = 0;
        let mut path = Vec::new();
        let (replacement, present) = loop {
            let Some(node) = link.read(&self.parent.cache)? else {
                if value.is_zero() {
                    return Ok(());
                }
                break (Link::leaf::<H>(key, value, preimage), false);
            };
            match &node.kind {
                Kind::Leaf { key: existing, value: old, .. } => {
                    if !same_prefix(existing, &key, depth) {
                        return Err(invalid_path());
                    }
                    if *existing == key {
                        if *old == value {
                            return Ok(());
                        }
                        let replacement = if value.is_zero() {
                            Link::Disk(B256::ZERO)
                        } else {
                            Link::leaf::<H>(key, value, preimage)
                        };
                        break (replacement, true);
                    }
                    if value.is_zero() {
                        return Ok(());
                    }
                    let split = (depth..existing.len().min(key.len()) * 8)
                        .find(|i| bit(existing, *i) != bit(&key, *i))
                        .ok_or_else(invalid_path)?;
                    let direction = bit(&key, split).ok_or_else(invalid_path)?;
                    let prefix = (depth..split).map(|i| bit(&key, i).unwrap_or_default()).collect();
                    break (
                        Link::branch::<H>(
                            prefix,
                            Link::leaf::<H>(key, value, preimage),
                            link,
                            direction,
                        ),
                        false,
                    );
                }
                Kind::Branch { prefix, left, right } => {
                    if depth + prefix.len() >= MAX_DEPTH {
                        return Err(invalid_path());
                    }
                    let matched = prefix
                        .iter()
                        .enumerate()
                        .take_while(|(i, b)| bit(&key, depth + i) == Some(**b))
                        .count();
                    if matched < prefix.len() && value.is_zero() {
                        return Ok(());
                    }
                    let direction = bit(&key, depth + matched).ok_or_else(invalid_path)?;
                    if matched < prefix.len() {
                        let survivor = Link::branch::<H>(
                            prefix[matched + 1..].to_vec(),
                            left.clone(),
                            right.clone(),
                            false,
                        );
                        break (
                            Link::branch::<H>(
                                prefix[..matched].to_vec(),
                                Link::leaf::<H>(key, value, preimage),
                                survivor,
                                direction,
                            ),
                            false,
                        );
                    }
                    depth += prefix.len() + 1;
                    link = if direction { right } else { left }.clone();
                    path.push((node, direction));
                }
            }
        };
        self.root = self.rebuild(replacement, path)?;
        if !present {
            self.leaves = self.leaves.checked_add(1).ok_or_else(invalid_path)?;
        }
        if value.is_zero() {
            self.leaves = self.leaves.checked_sub(1).ok_or_else(invalid_path)?;
        }
        Ok(())
    }

    fn rebuild(&self, mut link: Link, path: Vec<(Arc<Node>, bool)>) -> Result<Link, NodeError> {
        for (node, direction) in path.into_iter().rev() {
            let Kind::Branch { prefix, left, right } = &node.kind else {
                return Err(invalid_path());
            };
            let sibling = if direction { left } else { right };
            link = if link.hash().is_zero() {
                let survivor = sibling.read(&self.parent.cache)?.ok_or_else(invalid_path)?;
                match &survivor.kind {
                    Kind::Leaf { .. } => sibling.clone(),
                    Kind::Branch { prefix: suffix, left, right } => {
                        let mut merged = prefix.clone();
                        merged.push(!direction);
                        merged.extend(suffix);
                        if merged.len() >= MAX_DEPTH {
                            return Err(invalid_path());
                        }
                        Link::branch::<H>(merged, left.clone(), right.clone(), false)
                    }
                }
            } else {
                Link::branch::<H>(prefix.clone(), link, sibling.clone(), direction)
            };
        }
        Ok(link)
    }
}

/// Errors from resolving or updating a native checkpoint.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    /// Reading or writing native nodes failed.
    #[error(transparent)]
    Database(#[from] NodeError),
    /// An executed account cannot be represented by the PBT.
    #[error(transparent)]
    State(#[from] StateError),
    /// The executor referenced unavailable prior code.
    #[error("missing or inconsistent bytecode for {0}")]
    MissingCode(B256),
    /// A checkpoint cannot be saved to a database that lacks its leased parent.
    #[error("PBT overlay belongs to a different database")]
    WrongDatabase,
    /// The update or configured limits require a full-state index.
    #[error("PBT update requires full-state metadata")]
    NeedsFullState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderCode {
    Absent,
    Empty,
    Contract(B256),
    Delegation(Address),
}

impl HeaderCode {
    fn keccak(self) -> Option<B256> {
        match self {
            Self::Absent => None,
            Self::Empty => Some(KECCAK256_EMPTY),
            Self::Contract(hash) => Some(hash),
            Self::Delegation(target) => {
                Some(keccak256(&key::encode_delegation(target)[..key::DELEGATION_SIZE]))
            }
        }
    }

    fn leaf(self) -> Option<(u8, B256)> {
        match self {
            Self::Absent => None,
            Self::Empty => Some((CODE_HASH_LEAF_KEY, KECCAK256_EMPTY)),
            Self::Contract(hash) => Some((CODE_HASH_LEAF_KEY, hash)),
            Self::Delegation(target) => Some((DELEGATION_LEAF_KEY, key::encode_delegation(target))),
        }
    }
}

#[derive(Clone, Debug)]
enum Link {
    Disk(B256),
    Changed(Arc<Node>),
}

impl Link {
    fn hash(&self) -> B256 {
        match self {
            Self::Disk(hash) => *hash,
            Self::Changed(node) => node.hash,
        }
    }

    fn read<H: TreeHasher>(&self, cache: &SubtreeCache<H>) -> Result<Option<Arc<Node>>, NodeError> {
        match self {
            Self::Disk(hash) if hash.is_zero() => Ok(None),
            Self::Disk(hash) => cache.read(*hash).map(Some),
            Self::Changed(node) => Ok(Some(node.clone())),
        }
    }

    fn leaf<H: TreeHasher>(key: Vec<u8>, value: B256, preimage: Option<Vec<u8>>) -> Self {
        Self::Changed(Arc::new(Node {
            hash: leaf_hash::<H>(&key, value),
            kind: Kind::Leaf { key, value, preimage },
        }))
    }

    fn branch<H: TreeHasher>(prefix: Vec<bool>, child: Self, sibling: Self, right: bool) -> Self {
        let (left, right) = if right { (sibling, child) } else { (child, sibling) };
        Self::Changed(Arc::new(Node {
            hash: branch_hash::<H>(&prefix, left.hash(), right.hash()),
            kind: Kind::Branch { prefix, left, right },
        }))
    }
}

#[derive(Debug)]
struct Node {
    hash: B256,
    kind: Kind,
}

impl Node {
    fn encode(&self) -> Vec<u8> {
        match &self.kind {
            Kind::Leaf { key, value, preimage } => {
                super::encode_leaf(key, *value, preimage.as_deref())
            }
            Kind::Branch { prefix, left, right } => {
                super::encode_branch(prefix, left.hash(), right.hash())
            }
        }
    }
}

#[derive(Debug)]
enum Kind {
    Leaf { key: Vec<u8>, value: B256, preimage: Option<Vec<u8>> },
    Branch { prefix: Vec<bool>, left: Link, right: Link },
}

fn same_prefix(left: &[u8], right: &[u8], bits: usize) -> bool {
    let bytes = bits / 8;
    left.len() * 8 >= bits
        && right.len() * 8 >= bits
        && left[..bytes] == right[..bytes]
        && (bits.is_multiple_of(8) || (left[bytes] ^ right[bytes]) >> (8 - bits % 8) == 0)
}

fn invalid_path() -> NodeError {
    ArtifactError::Invalid("invalid native PBT path or account header").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountData, Blake3Hasher};

    type Cache = SubtreeCache<Blake3Hasher>;

    fn account(balance: u64, code: Bytes) -> AccountData {
        AccountData { nonce: 1, balance: U256::from(balance), code }
    }

    fn update(account: &AccountData) -> AccountUpdate {
        AccountUpdate {
            nonce: account.nonce,
            balance: account.balance,
            code: CodeUpdate::Bytes(account.code.clone()),
        }
    }

    #[test]
    fn overlay_matches_eager_roots_and_reopens_after_collection() {
        let directory = tempfile::tempdir().unwrap();
        let a = Address::repeat_byte(1);
        let b = Address::repeat_byte(2);
        let c = Address::repeat_byte(3);
        let code = Bytes::from(vec![0x7f; 31 * 260]);
        let mut original = StateTree::<Blake3Hasher>::new();
        original.set_account(a, account(1, code.clone())).unwrap();
        original.set_account(b, account(2, Bytes::new())).unwrap();
        for address in [a, b] {
            for i in [0, 1, 63, 64, 65, 255, 256, 257, u64::MAX] {
                original.set_storage(address, U256::from(i), B256::repeat_byte(1)).unwrap();
            }
        }
        let base = original.root();
        let mut expected = original.clone();
        let db = NodeDatabase::open(directory.path()).unwrap();
        let cache = Arc::new(Cache::new(db.clone(), 17));
        let limits = DecodeLimits::native();
        let parent = {
            let _guard = db.mutation.lock().unwrap();
            db.save(&original, limits).unwrap();
            cache.checkpoint(base).unwrap()
        };
        let mut changed = parent.overlay();
        let sibling = parent.overlay();
        assert!(matches!(
            changed.update_account(a, None, false, []),
            Err(OverlayError::NeedsFullState)
        ));
        assert!(matches!(
            changed.update_account(
                a,
                Some(AccountUpdate {
                    nonce: 2,
                    balance: U256::from(8),
                    code: CodeUpdate::Retain(B256::repeat_byte(0xfe))
                }),
                false,
                []
            ),
            Err(OverlayError::MissingCode(_))
        ));
        assert_eq!(changed.root(), base);
        for step in 0..90u64 {
            let address = if step % 2 == 0 { a } else { b };
            let data = account(step + 1, if address == a { code.clone() } else { Bytes::new() });
            let wipe = step % 5 == 0;
            let slots = [
                (U256::from(step % 67), B256::repeat_byte(5)),
                (U256::from(256 + step / 2), B256::repeat_byte(7)),
                (U256::from(64), B256::ZERO),
            ];
            if wipe {
                expected.clear_storage(address);
            }
            expected.set_account(address, data.clone()).unwrap();
            for (slot, value) in slots {
                expected.set_storage(address, slot, value).unwrap();
            }
            let change = if address == a {
                AccountUpdate {
                    nonce: data.nonce,
                    balance: data.balance,
                    code: CodeUpdate::Retain(keccak256(&code)),
                }
            } else {
                update(&data)
            };
            changed.update_account(address, Some(change), wipe, slots).unwrap();
            assert_eq!(changed.root(), expected.root(), "step {step}");
            assert_eq!(changed.leaf_count(), expected.tree().len());
        }
        let delegated =
            account(10, Bytes::copy_from_slice(&key::encode_delegation(a)[..key::DELEGATION_SIZE]));
        changed.update_account(b, Some(update(&delegated)), false, []).unwrap();
        expected.set_account(b, delegated).unwrap();
        let new_owner = account(2, code);
        changed.update_account(c, Some(update(&new_owner)), false, []).unwrap();
        expected.set_account(c, new_owner).unwrap();
        assert_eq!(changed.root(), expected.root());
        changed.update_account(b, None, false, []).unwrap();
        expected.remove_account(b);
        assert_eq!(changed.root(), expected.root());
        assert_eq!(sibling.root(), base);
        let root = changed.root();
        let count = changed.leaf_count();
        assert_eq!(db.leaf_count(root).unwrap(), None);
        assert!(db.is_pinned(base).unwrap());
        let mut published = false;
        db.collect_cooperatively_with::<Blake3Hasher>(256, |phase| {
            if matches!(phase, super::super::CollectionPhase::Swept) && !published {
                let _guard = db.mutation.lock().unwrap();
                changed.save(&db, limits).unwrap();
                published = true;
            }
            Ok(())
        })
        .unwrap();
        assert!(published);
        assert_eq!(db.leaf_count(root).unwrap(), Some(count));
        assert_eq!(cache.shards.iter().map(|shard| shard.lock().unwrap().len()).sum::<usize>(), 17);
        drop((parent, changed, sibling, cache));
        {
            let _guard = db.mutation.lock().unwrap();
            db.remove_root(base).unwrap();
            db.collect::<Blake3Hasher>().unwrap();
        }
        drop(db);
        let reopened = NodeDatabase::open(directory.path()).unwrap();
        let restored = reopened.load::<Blake3Hasher>(root, count, limits).unwrap();
        assert_eq!(restored.root(), expected.root());
        assert!(restored.tree().entries().eq(expected.tree().entries()));
        assert_eq!(restored.account(c), expected.account(c));
    }

    #[test]
    fn cold_update_reads_only_its_paths_and_refuses_wrong_database() {
        let directory = tempfile::tempdir().unwrap();
        let other_directory = tempfile::tempdir().unwrap();
        let address = Address::repeat_byte(7);
        let other = Address::repeat_byte(8);
        let data = account(10, Bytes::new());
        let mut original = StateTree::<Blake3Hasher>::new();
        original.set_account(address, data.clone()).unwrap();
        original.set_account(other, data.clone()).unwrap();
        original.set_storage(other, U256::from(100), B256::repeat_byte(12)).unwrap();
        let db = NodeDatabase::open(directory.path()).unwrap();
        let other_db = NodeDatabase::open(other_directory.path()).unwrap();
        let cache = Arc::new(Cache::new(db.clone(), 0));
        let base = original.root();
        let count = original.tree().len();
        let limits = DecodeLimits::native();
        let _guard = db.mutation.lock().unwrap();
        db.save(&original, limits).unwrap();
        let mut overlay = cache.checkpoint(base).unwrap().overlay();
        let storage = key::storage_key::<Blake3Hasher>(other, U256::from(100));
        let unrelated = leaf_hash::<Blake3Hasher>(&storage, B256::repeat_byte(12));
        db.db.delete(database_key(NODE, unrelated)).unwrap();
        let next = account(11, Bytes::new());
        overlay.update_account(address, Some(update(&next)), false, []).unwrap();
        original.set_account(address, next).unwrap();
        assert_eq!(overlay.root(), original.root());
        assert!(matches!(overlay.save(&other_db, limits), Err(OverlayError::WrongDatabase)));
        assert!(matches!(
            db.load::<Blake3Hasher>(base, count, limits),
            Err(NodeError::Artifact(ArtifactError::Invalid("missing PBT node")))
        ));
        let before = overlay.root();
        assert!(matches!(
            overlay.update_account(other, Some(update(&data)), true, []),
            Err(OverlayError::Database(NodeError::Artifact(ArtifactError::Invalid(
                "missing PBT node"
            ))))
        ));
        assert_eq!(overlay.root(), before);
    }
}
