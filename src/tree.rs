use crate::{
    Blake3Hasher, TreeHasher,
    hash::{branch_hash, leaf_hash},
};
use alloy_primitives::B256;
use std::{fmt, marker::PhantomData, sync::Arc};

/// A persistent, path-compressed binary Merkle tree.
///
/// Clones share immutable nodes. A mutation copies and rehashes only its search path, so a
/// previous clone remains an exact snapshot suitable for a competing block or a reorg.
#[derive(Clone)]
pub struct BinaryTree<H = Blake3Hasher> {
    pub(crate) root: Option<Arc<Node>>,
    len: usize,
    hasher: PhantomData<H>,
}

impl<H> fmt::Debug for BinaryTree<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BinaryTree")
            .field("root", &self.root.as_ref().map(|node| node.hash()))
            .field("len", &self.len)
            .finish()
    }
}

impl<H> Drop for BinaryTree<H> {
    fn drop(&mut self) {
        release(self.root.take());
    }
}

// Every owned root is released through this path, including replaced mutation roots.
// into_inner also handles snapshots being dropped concurrently: exactly one owner
// receives each last node, rather than letting Arc recursively destroy its children.
fn release(root: Option<Arc<Node>>) {
    let Some(Node::Branch { left, right, .. }) = root.and_then(Arc::into_inner) else {
        return;
    };
    let mut stack = vec![left, right];
    while let Some(node) = stack.pop() {
        if let Some(Node::Branch { left, right, .. }) = Arc::into_inner(node) {
            stack.push(left);
            stack.push(right);
        }
    }
}

impl<H: TreeHasher> Default for BinaryTree<H> {
    fn default() -> Self {
        Self { root: None, len: 0, hasher: PhantomData }
    }
}

impl<H: TreeHasher> BinaryTree<H> {
    /// Creates an empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the commitment, or 32 zero bytes for an empty tree.
    pub fn root(&self) -> B256 {
        self.root.as_ref().map_or(B256::ZERO, |node| node.hash())
    }

    /// Returns the number of leaves, including any explicitly stored zero values.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the tree has no leaves.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Looks up the exact key. A prefix of a stored key is not a match.
    pub fn get(&self, key: &[u8]) -> Option<B256> {
        let mut node = self.root.as_deref()?;
        let mut depth = 0;
        loop {
            match node {
                Node::Leaf { key: stored, value, .. } => {
                    return (stored.as_ref() == key).then_some(*value);
                }
                Node::Branch { prefix, left, right, .. } => {
                    if !matches_prefix(key, depth, prefix) {
                        return None;
                    }
                    depth += prefix.len();
                    node = if bit(key, depth)? { right } else { left };
                    depth += 1;
                }
            }
        }
    }

    /// Inserts or replaces a leaf, returning its previous value.
    ///
    /// Rejects empty, oversized, or non-prefix-free keys without changing the tree.
    pub fn insert(
        &mut self,
        key: impl Into<Vec<u8>>,
        value: B256,
    ) -> Result<Option<B256>, TreeError> {
        let key = key.into();
        validate_key(&key)?;
        let (root, previous) = match &self.root {
            None => (Node::leaf::<H>(key.into_boxed_slice(), value), None),
            Some(root) => insert::<H>(root, &key, value)?,
        };
        release(self.root.replace(root));
        self.len += usize::from(previous.is_none());
        Ok(previous)
    }

    /// Removes a leaf and collapses its parent branch into the surviving child.
    pub fn remove(&mut self, key: &[u8]) -> Option<B256> {
        let root = self.root.as_ref()?;
        let (new_root, previous) = remove::<H>(root, key);
        release(std::mem::replace(&mut self.root, new_root));
        self.len -= usize::from(previous.is_some());
        previous
    }

    /// Atomically applies final leaf values, with `None` deleting a key.
    ///
    /// Repeated keys use their last update. Dense batches rebuild sorted leaves
    /// once; small batches retain incremental path copying. Prefix validation is
    /// applied to the final set, so a batch can replace a key with its descendants.
    pub fn apply_updates(
        &mut self,
        updates: impl IntoIterator<Item = (Vec<u8>, Option<B256>)>,
    ) -> Result<(), TreeError> {
        let updates: std::collections::BTreeMap<_, _> = updates.into_iter().collect();
        for key in updates.keys() {
            validate_key(key)?;
        }
        if updates.is_empty() {
            return Ok(());
        }
        // Rebuilding the whole tree only pays off for sufficiently dense batches.
        // Keep smaller block updates incremental even when they touch many leaves.
        if self.is_empty() || (updates.len() >= 64 && updates.len().saturating_mul(4) >= self.len) {
            let mut entries: std::collections::BTreeMap<_, _> =
                self.entries().map(|(key, value)| (key.to_vec(), value)).collect();
            for (key, value) in updates {
                if let Some(value) = value {
                    entries.insert(key, value);
                } else {
                    entries.remove(&key);
                }
            }
            *self = Self::from_sorted_entries(entries)?;
        } else {
            let mut next = self.clone();
            for (key, value) in &updates {
                if value.is_none() {
                    next.remove(key);
                }
            }
            for (key, value) in updates {
                if let Some(value) = value {
                    next.insert(key, value)?;
                }
            }
            *self = next;
        }
        Ok(())
    }

    /// Builds a tree from strictly ascending, unique, prefix-free keys.
    pub fn from_sorted_entries(
        entries: impl IntoIterator<Item = (Vec<u8>, B256)>,
    ) -> Result<Self, TreeError> {
        let entries: Vec<_> = entries.into_iter().collect();
        validate_entries(&entries)?;
        Ok(Self {
            root: (!entries.is_empty()).then(|| build::<H>(&entries, 0)),
            len: entries.len(),
            hasher: PhantomData,
        })
    }

    /// Builds independent sorted subtrees in parallel, producing identical roots.
    /// Available only with the opt-in `experimental` feature. Small inputs use
    /// the serial builder; parallel recursion is bounded independently of key depth.
    #[cfg(feature = "experimental")]
    pub fn from_sorted_entries_parallel(
        entries: impl IntoIterator<Item = (Vec<u8>, B256)>,
    ) -> Result<Self, TreeError> {
        let entries: Vec<_> = entries.into_iter().collect();
        validate_entries(&entries)?;
        let budget = rayon::current_num_threads().next_power_of_two().trailing_zeros() as usize;
        Ok(Self {
            root: (!entries.is_empty()).then(|| build_parallel::<H>(&entries, 0, budget)),
            len: entries.len(),
            hasher: PhantomData,
        })
    }

    /// Visits leaves in ascending byte-key order without cloning keys.
    pub fn entries(&self) -> impl Iterator<Item = (&[u8], B256)> {
        let mut stack: Vec<&Node> = self.root.as_deref().into_iter().collect();
        std::iter::from_fn(move || {
            while let Some(node) = stack.pop() {
                match node {
                    Node::Leaf { key, value, .. } => return Some((key.as_ref(), *value)),
                    Node::Branch { left, right, .. } => {
                        stack.push(right);
                        stack.push(left);
                    }
                }
            }
            None
        })
    }
}

pub(crate) fn validate_entries(entries: &[(Vec<u8>, B256)]) -> Result<(), TreeError> {
    for (index, (key, _)) in entries.iter().enumerate() {
        validate_key(key)?;
        if index > 0 {
            let previous = &entries[index - 1].0;
            if previous >= key {
                return Err(TreeError::UnsortedKeys);
            }
            if key.starts_with(previous) {
                return Err(TreeError::PrefixConflict);
            }
        }
    }
    Ok(())
}

/// Maximum generic key length allowed by the two-byte branch-prefix bit count.
pub const MAX_KEY_LENGTH: usize = 8192;

/// Invalid tree input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// A key is empty or exceeds [`MAX_KEY_LENGTH`].
    #[error("key must contain 1..={MAX_KEY_LENGTH} bytes")]
    InvalidKeyLength,
    /// One key is a proper prefix of another.
    #[error("tree keys must be prefix-free")]
    PrefixConflict,
    /// Bulk input is not strictly ascending or contains duplicates.
    #[error("bulk keys must be strictly ascending and unique")]
    UnsortedKeys,
}

#[derive(Debug)]
pub(crate) enum Node {
    Leaf { key: Box<[u8]>, value: B256, hash: B256 },
    Branch { prefix: Vec<bool>, left: Arc<Self>, right: Arc<Self>, hash: B256 },
}

impl Node {
    pub(crate) const fn hash(&self) -> B256 {
        match self {
            Self::Leaf { hash, .. } | Self::Branch { hash, .. } => *hash,
        }
    }

    fn leaf<H: TreeHasher>(key: Box<[u8]>, value: B256) -> Arc<Self> {
        let hash = leaf_hash::<H>(&key, value);
        Arc::new(Self::Leaf { key, value, hash })
    }

    fn branch<H: TreeHasher>(prefix: Vec<bool>, left: Arc<Self>, right: Arc<Self>) -> Arc<Self> {
        let hash = branch_hash::<H>(&prefix, left.hash(), right.hash());
        Arc::new(Self::Branch { prefix, left, right, hash })
    }
}

pub(crate) const fn validate_key(key: &[u8]) -> Result<(), TreeError> {
    if key.is_empty() || key.len() > MAX_KEY_LENGTH {
        return Err(TreeError::InvalidKeyLength);
    }
    Ok(())
}

pub(crate) fn bit(key: &[u8], index: usize) -> Option<bool> {
    key.get(index / 8).map(|byte| byte & (1 << (7 - index % 8)) != 0)
}

pub(crate) fn matches_prefix(key: &[u8], depth: usize, prefix: &[bool]) -> bool {
    prefix.iter().enumerate().all(|(i, value)| bit(key, depth + i) == Some(*value))
}

fn insert<H: TreeHasher>(
    root: &Arc<Node>,
    key: &[u8],
    value: B256,
) -> Result<(Arc<Node>, Option<B256>), TreeError> {
    let mut node = root;
    let mut depth = 0;
    let mut path = Vec::new();
    let (mut replacement, previous) = loop {
        match node.as_ref() {
            Node::Leaf { key: stored, value: previous, .. } => {
                if stored.as_ref() == key {
                    if *previous == value {
                        return Ok((root.clone(), Some(*previous)));
                    }
                    break (Node::leaf::<H>(key.into(), value), Some(*previous));
                }
                let limit = key.len().min(stored.len()) * 8;
                let split = (depth..limit)
                    .find(|i| bit(key, *i) != bit(stored, *i))
                    .ok_or(TreeError::PrefixConflict)?;
                let prefix = (depth..split).map(|i| bit(key, i).unwrap()).collect();
                let leaf = Node::leaf::<H>(key.into(), value);
                let (left, right) = if bit(key, split).unwrap() {
                    (node.clone(), leaf)
                } else {
                    (leaf, node.clone())
                };
                break (Node::branch::<H>(prefix, left, right), None);
            }
            Node::Branch { prefix, left, right, .. } => {
                let matched = prefix
                    .iter()
                    .enumerate()
                    .take_while(|(index, value)| bit(key, depth + index) == Some(**value))
                    .count();
                let direction = bit(key, depth + matched).ok_or(TreeError::PrefixConflict)?;
                if matched < prefix.len() {
                    let survivor = Node::branch::<H>(
                        prefix[matched + 1..].to_vec(),
                        left.clone(),
                        right.clone(),
                    );
                    let leaf = Node::leaf::<H>(key.into(), value);
                    let (left, right) = if direction { (survivor, leaf) } else { (leaf, survivor) };
                    break (Node::branch::<H>(prefix[..matched].to_vec(), left, right), None);
                }
                path.push((prefix, if direction { left } else { right }, direction));
                node = if direction { right } else { left };
                depth += matched + 1;
            }
        }
    };
    while let Some((prefix, sibling, direction)) = path.pop() {
        let (left, right) =
            if direction { (sibling.clone(), replacement) } else { (replacement, sibling.clone()) };
        replacement = Node::branch::<H>(prefix.clone(), left, right);
    }
    Ok((replacement, previous))
}

fn remove<H: TreeHasher>(root: &Arc<Node>, key: &[u8]) -> (Option<Arc<Node>>, Option<B256>) {
    let mut node = root;
    let mut depth = 0;
    let mut path = Vec::new();
    let previous = loop {
        match node.as_ref() {
            Node::Leaf { key: stored, value, .. } => {
                if stored.as_ref() != key {
                    return (Some(root.clone()), None);
                }
                break *value;
            }
            Node::Branch { prefix, left, right, .. } => {
                if !matches_prefix(key, depth, prefix) {
                    return (Some(root.clone()), None);
                }
                let Some(direction) = bit(key, depth + prefix.len()) else {
                    return (Some(root.clone()), None);
                };
                path.push((prefix, if direction { left } else { right }, direction));
                node = if direction { right } else { left };
                depth += prefix.len() + 1;
            }
        }
    };
    let mut replacement = None;
    while let Some((prefix, sibling, direction)) = path.pop() {
        replacement = Some(if let Some(child) = replacement {
            let (left, right) =
                if direction { (sibling.clone(), child) } else { (child, sibling.clone()) };
            Node::branch::<H>(prefix.clone(), left, right)
        } else {
            match sibling.as_ref() {
                Node::Leaf { .. } => sibling.clone(),
                Node::Branch { prefix: child_prefix, left, right, .. } => {
                    let mut merged = prefix.clone();
                    merged.push(!direction);
                    merged.extend(child_prefix);
                    Node::branch::<H>(merged, left.clone(), right.clone())
                }
            }
        });
    }
    (replacement, Some(previous))
}

fn build<H: TreeHasher>(entries: &[(Vec<u8>, B256)], depth: usize) -> Arc<Node> {
    enum Task<'a> {
        Build(&'a [(Vec<u8>, B256)], usize),
        Branch(Vec<bool>),
    }
    let mut tasks = vec![Task::Build(entries, depth)];
    let mut nodes = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Build(entries, depth) => {
                if entries.len() == 1 {
                    nodes.push(Node::leaf::<H>(
                        entries[0].0.clone().into_boxed_slice(),
                        entries[0].1,
                    ));
                    continue;
                }
                let first = &entries[0].0;
                let last = &entries[entries.len() - 1].0;
                let split = (depth..first.len().min(last.len()) * 8)
                    .find(|i| bit(first, *i) != bit(last, *i))
                    .expect("validated prefix-free keys");
                let middle = entries.partition_point(|(key, _)| !bit(key, split).unwrap());
                let prefix = (depth..split).map(|i| bit(first, i).unwrap()).collect();
                tasks.push(Task::Branch(prefix));
                tasks.push(Task::Build(&entries[middle..], split + 1));
                tasks.push(Task::Build(&entries[..middle], split + 1));
            }
            Task::Branch(prefix) => {
                let right = nodes.pop().unwrap();
                let left = nodes.pop().unwrap();
                nodes.push(Node::branch::<H>(prefix, left, right));
            }
        }
    }
    nodes.pop().unwrap()
}

#[cfg(feature = "experimental")]
fn build_parallel<H: TreeHasher>(
    entries: &[(Vec<u8>, B256)],
    depth: usize,
    budget: usize,
) -> Arc<Node> {
    if entries.len() < 4096 || budget == 0 {
        return build::<H>(entries, depth);
    }
    let first = &entries[0].0;
    let last = &entries[entries.len() - 1].0;
    let split = (depth..first.len().min(last.len()) * 8)
        .find(|index| bit(first, *index) != bit(last, *index))
        .expect("validated prefix-free keys");
    let middle = entries.partition_point(|(key, _)| !bit(key, split).unwrap());
    let prefix = (depth..split).map(|index| bit(first, index).unwrap()).collect();
    let (left, right) = rayon::join(
        || build_parallel::<H>(&entries[..middle], split + 1, budget - 1),
        || build_parallel::<H>(&entries[middle..], split + 1, budget - 1),
    );
    Node::branch::<H>(prefix, left, right)
}
