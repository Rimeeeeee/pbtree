#![no_main]

use alloy_primitives::B256;
use libfuzzer_sys::fuzz_target;
use pbt::{BinaryTree, Blake3Hasher, ProofTerminal, TreeError, TreeHasher};
use std::collections::BTreeMap;

type Tree = BinaryTree<Blake3Hasher>;
type Model = BTreeMap<Vec<u8>, B256>;

fn bit(key: &[u8], index: usize) -> bool {
    key[index / 8] & (0x80 >> (index % 8)) != 0
}

// Independent root construction: partition all entries by their first differing
// bit and encode the commitment directly, without using BinaryTree or its builder.
// The input format limits keys to eight bytes, bounding this oracle's recursion.
fn reference(entries: &[(&Vec<u8>, &B256)], depth: usize) -> B256 {
    if entries.is_empty() {
        return B256::ZERO;
    }
    if entries.len() == 1 {
        return Blake3Hasher::hash(&[&[0], entries[0].0, entries[0].1.as_slice()]);
    }
    let first = entries[0].0;
    let mut split = depth;
    while entries.iter().all(|(key, _)| bit(key, split) == bit(first, split)) {
        split += 1;
    }
    let mut left = Vec::new();
    let mut right = Vec::new();
    for entry in entries {
        if bit(entry.0, split) {
            right.push(*entry);
        } else {
            left.push(*entry);
        }
    }
    let count = split - depth;
    let mut prefix = vec![0; 2 + count.div_ceil(8)];
    prefix[..2].copy_from_slice(&(count as u16).to_be_bytes());
    for index in 0..count {
        if bit(first, depth + index) {
            prefix[2 + index / 8] |= 0x80 >> (index % 8);
        }
    }
    let left = reference(&left, split + 1);
    let right = reference(&right, split + 1);
    Blake3Hasher::hash(&[&[1], &prefix, left.as_slice(), right.as_slice()])
}

fn check_proof(tree: &Tree, model: &Model, key: &[u8], selector: usize) {
    let proof = tree.proof(key);
    assert_eq!(proof.verify::<Blake3Hasher>(tree.root(), key).unwrap(), model.get(key).copied());
    if !proof.steps.is_empty() {
        let index = selector % proof.steps.len();
        let mut tampered = proof.clone();
        tampered.steps[index].sibling[0] ^= 1;
        assert!(tampered.verify::<Blake3Hasher>(tree.root(), key).is_err());
        let mut tampered = proof.clone();
        tampered.steps[index].right = !tampered.steps[index].right;
        assert!(tampered.verify::<Blake3Hasher>(tree.root(), key).is_err());
    }
    let mut tampered = proof;
    match &mut tampered.terminal {
        ProofTerminal::Empty => return,
        ProofTerminal::Leaf { value, .. } => value[0] ^= 1,
        ProofTerminal::Branch { left, .. } => left[0] ^= 1,
    }
    assert!(tampered.verify::<Blake3Hasher>(tree.root(), key).is_err());
}

fuzz_target!(|data: &[u8]| {
    let mut tree = Tree::new();
    let mut model = Model::new();
    for record in data.chunks(42).take(128) {
        let control = record[0];
        let key_len = 1 + usize::from(record.get(1).copied().unwrap_or(0)) % 8;
        let mut key: Vec<_> =
            (0..key_len).map(|index| record.get(2 + index).copied().unwrap_or(0)).collect();
        if control & 8 != 0 && !model.is_empty() {
            let index = usize::from(record.get(2).copied().unwrap_or(0)) % model.len();
            key = model.keys().nth(index).unwrap().clone();
        }
        let snapshot = tree.clone();
        let previous_root = tree.root();
        let previous_entries = model.clone();
        match control % 4 {
            0 | 1 => {
                let value = B256::from(std::array::from_fn(|index| {
                    record.get(10 + index).copied().unwrap_or(0)
                }));
                let conflict = model.keys().any(|other| {
                    other != &key && (other.starts_with(&key) || key.starts_with(other))
                });
                if conflict {
                    assert_eq!(tree.insert(key.clone(), value), Err(TreeError::PrefixConflict));
                } else {
                    assert_eq!(
                        tree.insert(key.clone(), value).unwrap(),
                        model.insert(key.clone(), value)
                    );
                }
            }
            2 => assert_eq!(tree.remove(&key), model.remove(&key)),
            _ => {}
        }
        assert_eq!(snapshot.root(), previous_root);
        assert_eq!(
            snapshot.entries().map(|(key, value)| (key.to_vec(), value)).collect::<Model>(),
            previous_entries
        );
        assert_eq!(tree.root(), reference(&model.iter().collect::<Vec<_>>(), 0));
        assert_eq!(tree.len(), model.len());
        assert_eq!(tree.get(&key), model.get(&key).copied());
        assert_eq!(
            tree.entries().map(|(key, value)| (key.to_vec(), value)).collect::<Model>(),
            model
        );
        check_proof(&tree, &model, &key, usize::from(control));
        // Exercise absence proofs too, even when this operation selected an existing key.
        key.push(record.last().copied().unwrap_or(0));
        check_proof(&tree, &model, &key, usize::from(control));
    }
});
