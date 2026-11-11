//! Opt-in append-only arena with compact node indices and immutable snapshot roots.
//!
//! Clones share the arena, not their root. Mutations append nodes, so old snapshots
//! stay exact. History is retained until the last clone drops, or a particular
//! snapshot calls [`ArenaTree::compact`] to move its reachable nodes into a new arena.
//! This backend is for measurement; the default tree and database format are unchanged.

use crate::{
    Blake3Hasher, Proof, ProofStep, ProofTerminal, TreeError, TreeHasher,
    hash::{leaf_hash, packed_branch_hash},
    prefix::Prefix,
    tree::{bit, validate_entries, validate_key},
};
use alloy_primitives::B256;
use std::{
    fmt,
    marker::PhantomData,
    sync::{Arc, RwLock},
};

type Id = u32;

#[derive(Debug)]
enum Node {
    Leaf { key: Box<[u8]>, value: B256, hash: B256 },
    Branch { prefix: Prefix, left: Id, right: Id, hash: B256 },
}

impl Node {
    const fn hash(&self) -> B256 {
        match self {
            Self::Leaf { hash, .. } | Self::Branch { hash, .. } => *hash,
        }
    }
}

/// A persistent root backed by a shared append-only node arena.
///
/// Reads share a lock; writers serialize arena appends. This avoids per-node
/// atomic reference counts, at the cost of retained history and a shared write lock.
#[derive(Clone)]
pub struct ArenaTree<H = Blake3Hasher> {
    nodes: Arc<RwLock<Vec<Node>>>,
    root: Option<Id>,
    commitment: B256,
    len: usize,
    hasher: PhantomData<H>,
}

impl<H> fmt::Debug for ArenaTree<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArenaTree").field("root", &self.commitment).field("len", &self.len).finish()
    }
}

/// Invalid input or an unavailable arena.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum ArenaError {
    /// The same key validation errors as the default tree.
    #[error(transparent)]
    Tree(#[from] TreeError),
    /// Appending another node would exceed the compact index space.
    #[error("arena node index capacity exhausted; compact the snapshot")]
    Capacity,
    /// A panicking writer poisoned the shared arena lock.
    #[error("arena lock poisoned")]
    Poisoned,
}

impl<H: TreeHasher> Default for ArenaTree<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: TreeHasher> ArenaTree<H> {
    /// Creates an empty arena snapshot.
    pub fn new() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(Vec::new())),
            root: None,
            commitment: B256::ZERO,
            len: 0,
            hasher: PhantomData,
        }
    }

    /// Returns the same commitment as the default tree for this leaf set.
    pub const fn root(&self) -> B256 {
        self.commitment
    }

    /// Returns the live leaf count of this snapshot.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this snapshot has no leaves.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Counts all arena records, including history retained by other roots.
    pub fn allocated_nodes(&self) -> Result<usize, ArenaError> {
        Ok(self.nodes.read().map_err(|_| ArenaError::Poisoned)?.len())
    }

    /// Looks up an exact key without affecting any shared snapshot.
    pub fn get(&self, key: &[u8]) -> Result<Option<B256>, ArenaError> {
        let nodes = self.nodes.read().map_err(|_| ArenaError::Poisoned)?;
        let Some(mut id) = self.root else { return Ok(None) };
        let mut depth = 0;
        loop {
            match &nodes[id as usize] {
                Node::Leaf { key: stored, value, .. } => {
                    return Ok((stored.as_ref() == key).then_some(*value));
                }
                Node::Branch { prefix, left, right, .. } => {
                    if !prefix.matches(key, depth) {
                        return Ok(None);
                    }
                    let Some(direction) = bit(key, depth + prefix.len()) else { return Ok(None) };
                    depth += prefix.len() + 1;
                    id = if direction { *right } else { *left };
                }
            }
        }
    }

    /// Inserts or replaces a leaf. Validation errors leave the root unchanged.
    pub fn insert(
        &mut self,
        key: impl Into<Vec<u8>>,
        value: B256,
    ) -> Result<Option<B256>, ArenaError> {
        let key = key.into();
        validate_key(&key)?;
        let mut nodes = self.nodes.write().map_err(|_| ArenaError::Poisoned)?;
        let mut path = Vec::new();
        let mut depth = 0;
        let mut current = self.root;
        let (mut replacement, previous) = loop {
            let Some(id) = current else { break (leaf::<H>(&mut nodes, &key, value)?, None) };
            match &nodes[id as usize] {
                Node::Leaf { key: stored, value: previous, .. } => {
                    if stored.as_ref() == key {
                        let previous = *previous;
                        if previous == value {
                            return Ok(Some(previous));
                        }
                        break (leaf::<H>(&mut nodes, &key, value)?, Some(previous));
                    }
                    let split = (depth..key.len().min(stored.len()) * 8)
                        .find(|index| bit(&key, *index) != bit(stored, *index))
                        .ok_or(TreeError::PrefixConflict)?;
                    let prefix = Prefix::new((depth..split).map(|index| bit(&key, index).unwrap()));
                    let new = leaf::<H>(&mut nodes, &key, value)?;
                    let (left, right) =
                        if bit(&key, split).unwrap() { (id, new) } else { (new, id) };
                    break (branch::<H>(&mut nodes, prefix, left, right)?, None);
                }
                Node::Branch { prefix, left, right, .. } => {
                    let matched = prefix.common_prefix(&key, depth);
                    let direction = bit(&key, depth + matched).ok_or(TreeError::PrefixConflict)?;
                    if matched < prefix.len() {
                        let parent = prefix.prefix(matched);
                        let remainder = prefix.suffix(matched + 1);
                        let (left, right) = (*left, *right);
                        let survivor = branch::<H>(&mut nodes, remainder, left, right)?;
                        let new = leaf::<H>(&mut nodes, &key, value)?;
                        let (left, right) =
                            if direction { (survivor, new) } else { (new, survivor) };
                        break (branch::<H>(&mut nodes, parent, left, right)?, None);
                    }
                    path.push((prefix.clone(), if direction { *left } else { *right }, direction));
                    current = Some(if direction { *right } else { *left });
                    depth += matched + 1;
                }
            }
        };
        while let Some((prefix, sibling, direction)) = path.pop() {
            let (left, right) =
                if direction { (sibling, replacement) } else { (replacement, sibling) };
            replacement = branch::<H>(&mut nodes, prefix, left, right)?;
        }
        self.root = Some(replacement);
        self.commitment = nodes[replacement as usize].hash();
        self.len += usize::from(previous.is_none());
        Ok(previous)
    }

    /// Removes a leaf, preserving older roots that share this arena.
    pub fn remove(&mut self, key: &[u8]) -> Result<Option<B256>, ArenaError> {
        let Some(mut id) = self.root else { return Ok(None) };
        let mut nodes = self.nodes.write().map_err(|_| ArenaError::Poisoned)?;
        let mut path = Vec::new();
        let mut depth = 0;
        let previous = loop {
            match &nodes[id as usize] {
                Node::Leaf { key: stored, value, .. } => {
                    if stored.as_ref() != key {
                        return Ok(None);
                    }
                    break *value;
                }
                Node::Branch { prefix, left, right, .. } => {
                    if !prefix.matches(key, depth) {
                        return Ok(None);
                    }
                    let Some(direction) = bit(key, depth + prefix.len()) else { return Ok(None) };
                    path.push((prefix.clone(), if direction { *left } else { *right }, direction));
                    id = if direction { *right } else { *left };
                    depth += prefix.len() + 1;
                }
            }
        };
        let mut replacement = None;
        while let Some((prefix, sibling, direction)) = path.pop() {
            replacement = Some(if let Some(child) = replacement {
                let (left, right) = if direction { (sibling, child) } else { (child, sibling) };
                branch::<H>(&mut nodes, prefix, left, right)?
            } else {
                match &nodes[sibling as usize] {
                    Node::Leaf { .. } => sibling,
                    Node::Branch { prefix: child, left, right, .. } => {
                        let merged = prefix.concat(!direction, child);
                        let (left, right) = (*left, *right);
                        branch::<H>(&mut nodes, merged, left, right)?
                    }
                }
            });
        }
        self.root = replacement;
        self.commitment = replacement.map_or(B256::ZERO, |id| nodes[id as usize].hash());
        self.len -= 1;
        Ok(Some(previous))
    }

    /// Copies live leaves into ascending key order.
    pub fn entries(&self) -> Result<Vec<(Vec<u8>, B256)>, ArenaError> {
        let nodes = self.nodes.read().map_err(|_| ArenaError::Poisoned)?;
        let mut stack: Vec<_> = self.root.into_iter().collect();
        let mut entries = Vec::with_capacity(self.len);
        while let Some(id) = stack.pop() {
            match &nodes[id as usize] {
                Node::Leaf { key, value, .. } => entries.push((key.to_vec(), *value)),
                Node::Branch { left, right, .. } => {
                    stack.push(*right);
                    stack.push(*left);
                }
            }
        }
        Ok(entries)
    }

    /// Produces a proof accepted by the default verifier.
    pub fn proof(&self, key: &[u8]) -> Result<Proof, ArenaError> {
        let nodes = self.nodes.read().map_err(|_| ArenaError::Poisoned)?;
        let Some(mut id) = self.root else {
            return Ok(Proof { steps: Vec::new(), terminal: ProofTerminal::Empty });
        };
        let mut steps = Vec::new();
        let mut depth = 0;
        let terminal = loop {
            match &nodes[id as usize] {
                Node::Leaf { key, value, .. } => {
                    break ProofTerminal::Leaf { key: key.to_vec(), value: *value };
                }
                Node::Branch { prefix, left, right, .. } => {
                    let direction = bit(key, depth + prefix.len());
                    if !prefix.matches(key, depth) || direction.is_none() {
                        break ProofTerminal::Branch {
                            prefix: prefix.to_vec(),
                            left: nodes[*left as usize].hash(),
                            right: nodes[*right as usize].hash(),
                        };
                    }
                    let direction = direction.unwrap();
                    steps.push(ProofStep {
                        prefix: prefix.to_vec(),
                        right: direction,
                        sibling: nodes[if direction { *left } else { *right } as usize].hash(),
                    });
                    id = if direction { *right } else { *left };
                    depth += prefix.len() + 1;
                }
            }
        };
        Ok(Proof { steps, terminal })
    }

    /// Builds sorted, unique, prefix-free leaves directly into contiguous storage.
    pub fn from_sorted_entries(
        entries: impl IntoIterator<Item = (Vec<u8>, B256)>,
    ) -> Result<Self, ArenaError> {
        let entries: Vec<_> = entries.into_iter().collect();
        validate_entries(&entries)?;
        enum Task<'a> {
            Build(&'a [(Vec<u8>, B256)], usize),
            Branch(Prefix),
        }
        let mut tasks = Vec::new();
        if !entries.is_empty() {
            tasks.push(Task::Build(&entries, 0));
        }
        let mut roots = Vec::new();
        let mut nodes = Vec::with_capacity(entries.len().saturating_mul(2).saturating_sub(1));
        while let Some(task) = tasks.pop() {
            match task {
                Task::Build(entries, depth) => {
                    if entries.len() == 1 {
                        roots.push(leaf::<H>(&mut nodes, &entries[0].0, entries[0].1)?);
                        continue;
                    }
                    let first = &entries[0].0;
                    let last = &entries[entries.len() - 1].0;
                    let split = (depth..first.len().min(last.len()) * 8)
                        .find(|index| bit(first, *index) != bit(last, *index))
                        .expect("validated keys");
                    let middle = entries.partition_point(|(key, _)| !bit(key, split).unwrap());
                    tasks.push(Task::Branch(Prefix::new(
                        (depth..split).map(|index| bit(first, index).unwrap()),
                    )));
                    tasks.push(Task::Build(&entries[middle..], split + 1));
                    tasks.push(Task::Build(&entries[..middle], split + 1));
                }
                Task::Branch(prefix) => {
                    let right = roots.pop().unwrap();
                    let left = roots.pop().unwrap();
                    roots.push(branch::<H>(&mut nodes, prefix, left, right)?);
                }
            }
        }
        let root = roots.pop();
        let commitment = root.map_or(B256::ZERO, |id| nodes[id as usize].hash());
        Ok(Self {
            nodes: Arc::new(RwLock::new(nodes)),
            root,
            commitment,
            len: entries.len(),
            hasher: PhantomData,
        })
    }

    /// Detaches this snapshot into a fresh arena containing only reachable nodes.
    /// Other snapshots keep their existing arena and roots.
    pub fn compact(&mut self) -> Result<(), ArenaError> {
        *self = Self::from_sorted_entries(self.entries()?)?;
        Ok(())
    }
}

fn push(nodes: &mut Vec<Node>, node: Node) -> Result<Id, ArenaError> {
    let id = Id::try_from(nodes.len()).map_err(|_| ArenaError::Capacity)?;
    nodes.push(node);
    Ok(id)
}

fn leaf<H: TreeHasher>(nodes: &mut Vec<Node>, key: &[u8], value: B256) -> Result<Id, ArenaError> {
    push(nodes, Node::Leaf { key: key.into(), value, hash: leaf_hash::<H>(key, value) })
}

fn branch<H: TreeHasher>(
    nodes: &mut Vec<Node>,
    prefix: Prefix,
    left: Id,
    right: Id,
) -> Result<Id, ArenaError> {
    let hash =
        packed_branch_hash::<H>(&prefix, nodes[left as usize].hash(), nodes[right as usize].hash());
    push(nodes, Node::Branch { prefix, left, right, hash })
}
