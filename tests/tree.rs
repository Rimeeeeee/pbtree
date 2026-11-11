//! Tree mutation, snapshot, and adversarial-proof tests.

use alloy_primitives::B256;
use pbt::{BinaryTree, Blake3Hasher, MAX_KEY_LENGTH, ProofError, ProofTerminal, TreeError};
use proptest::{collection, prelude::*};
use std::collections::BTreeMap;

fn tree() -> BinaryTree {
    BinaryTree::new()
}

#[test]
fn prefix_conflicts_leave_snapshot_unchanged() {
    for (first, second) in [(vec![1], vec![1, 2]), (vec![1, 2], vec![1])] {
        let mut tree = tree();
        tree.insert(first, B256::ZERO).unwrap();
        let root = tree.root();
        assert_eq!(tree.insert(second, B256::repeat_byte(1)), Err(TreeError::PrefixConflict));
        assert_eq!(tree.root(), root);
        assert_eq!(tree.len(), 1);
    }
    let mut tree = tree();
    tree.insert(vec![1, 0], B256::ZERO).unwrap();
    tree.insert(vec![1, 128], B256::ZERO).unwrap();
    let root = tree.root();
    assert_eq!(tree.insert(vec![1], B256::ZERO), Err(TreeError::PrefixConflict));
    assert_eq!(tree.root(), root);
}

#[test]
fn maximum_prefix_and_key_length() {
    let mut tree = tree();
    assert_eq!(tree.insert(vec![], B256::ZERO), Err(TreeError::InvalidKeyLength));
    assert_eq!(
        tree.insert(vec![0; MAX_KEY_LENGTH + 1], B256::ZERO),
        Err(TreeError::InvalidKeyLength)
    );
    let a = vec![0; MAX_KEY_LENGTH];
    let mut b = a.clone();
    b[MAX_KEY_LENGTH - 1] = 1;
    tree.insert(a.clone(), B256::ZERO).unwrap();
    let snapshot = tree.clone();
    tree.insert(b.clone(), B256::repeat_byte(1)).unwrap();
    assert_eq!(tree.proof(&a).verify::<Blake3Hasher>(tree.root(), &a), Ok(Some(B256::ZERO)));
    tree.remove(&b);
    assert_eq!(tree.root(), snapshot.root());
}

#[test]
fn deep_comb_build_mutations_and_concurrent_drop_use_bounded_stack() {
    // Each key diverges one bit later, producing 8192 nested branches. Keep the
    // worker stack small so recursion cannot hide behind the test runner's stack.
    let snapshot = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            const DEPTH: usize = 8192;
            let key = vec![0; DEPTH / 8];
            let entries =
                std::iter::once((key.clone(), B256::ZERO)).chain((0..DEPTH).rev().map(|bit| {
                    let mut sibling = key.clone();
                    sibling[bit / 8] |= 1 << (7 - bit % 8);
                    (sibling, B256::ZERO)
                }));
            let mut tree = BinaryTree::<Blake3Hasher>::from_sorted_entries(entries).unwrap();
            let snapshot = tree.clone();
            assert_eq!(tree.insert(key.clone(), B256::repeat_byte(1)).unwrap(), Some(B256::ZERO));
            assert_eq!(tree.remove(&key), Some(B256::repeat_byte(1)));
            assert_eq!(tree.insert(key.clone(), B256::ZERO).unwrap(), None);
            assert_eq!(tree.root(), snapshot.root());
            assert_eq!(snapshot.get(&key), Some(B256::ZERO));
            assert_eq!(
                tree.proof(&key).verify::<Blake3Hasher>(tree.root(), &key),
                Ok(Some(B256::ZERO))
            );
            assert!(!format!("{tree:?}").is_empty());
            drop(tree);
            snapshot
        })
        .unwrap()
        .join()
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = [snapshot.clone(), snapshot]
        .into_iter()
        .map(|tree| {
            let barrier = barrier.clone();
            std::thread::Builder::new()
                .stack_size(64 * 1024)
                .spawn(move || {
                    barrier.wait();
                    drop(tree);
                })
                .unwrap()
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn bulk_input_is_strictly_validated() {
    for keys in [vec![vec![2], vec![1]], vec![vec![1], vec![1]]] {
        assert_eq!(
            BinaryTree::<Blake3Hasher>::from_sorted_entries(
                keys.into_iter().map(|key| (key, B256::ZERO))
            )
            .unwrap_err(),
            TreeError::UnsortedKeys
        );
    }
    assert_eq!(
        BinaryTree::<Blake3Hasher>::from_sorted_entries([
            (vec![1], B256::ZERO),
            (vec![1, 2], B256::ZERO)
        ])
        .unwrap_err(),
        TreeError::PrefixConflict
    );
}

#[test]
fn absence_and_tampered_proofs() {
    let mut tree = tree();
    assert_eq!(tree.proof(&[0]).verify::<Blake3Hasher>(tree.root(), &[0]), Ok(None));
    for key in [vec![0, 0], vec![0, 1], vec![128, 0]] {
        tree.insert(key, B256::repeat_byte(7)).unwrap();
    }
    for key in [vec![0], vec![0, 2], vec![128, 1], vec![255]] {
        assert_eq!(tree.proof(&key).verify::<Blake3Hasher>(tree.root(), &key), Ok(None));
    }
    let mut proof = tree.proof(&[0, 0]);
    proof.steps[0].sibling = B256::ZERO;
    assert_eq!(proof.verify::<Blake3Hasher>(tree.root(), &[0, 0]), Err(ProofError::RootMismatch));
    let mut proof = tree.proof(&[0, 0]);
    proof.steps[0].right = !proof.steps[0].right;
    assert_eq!(proof.verify::<Blake3Hasher>(tree.root(), &[0, 0]), Err(ProofError::InvalidPath));
    let mut proof = tree.proof(&[0, 0]);
    proof.terminal = ProofTerminal::Empty;
    assert_eq!(proof.verify::<Blake3Hasher>(tree.root(), &[0, 0]), Err(ProofError::InvalidPath));
}

#[test]
fn batches_validate_the_final_prefix_free_set_atomically() {
    let mut tree = tree();
    tree.insert(vec![1, 2], B256::repeat_byte(1)).unwrap();
    let old = tree.clone();
    tree.apply_updates([(vec![1], Some(B256::ZERO)), (vec![1, 2], None)]).unwrap();
    assert_eq!(tree.entries().collect::<Vec<_>>(), vec![(&[1][..], B256::ZERO)]);
    let before = tree.root();
    assert_eq!(
        tree.apply_updates([(vec![1, 2], Some(B256::ZERO))]),
        Err(TreeError::PrefixConflict)
    );
    assert_eq!(tree.root(), before);
    assert_eq!(old.get(&[1, 2]), Some(B256::repeat_byte(1)));
}

proptest! {
    #[test]
    fn adaptive_batches_match_final_map(
        initial in collection::btree_map(any::<[u8; 2]>(), any::<[u8; 32]>(), 0..600),
        updates in collection::vec((any::<[u8; 2]>(), proptest::option::of(any::<[u8; 32]>())), 0..200),
    ) {
        let mut model: BTreeMap<_, _> = initial.into_iter().map(|(key, value)| (key.to_vec(), B256::from(value))).collect();
        let mut tree = BinaryTree::<Blake3Hasher>::from_sorted_entries(model.clone()).unwrap();
        let snapshot = tree.clone();
        let before = snapshot.root();
        let changes: Vec<_> = updates.into_iter().map(|(key, value)| (key.to_vec(), value.map(B256::from))).collect();
        for (key, value) in &changes {
            if let Some(value) = value { model.insert(key.clone(), *value); } else { model.remove(key); }
        }
        tree.apply_updates(changes).unwrap();
        let rebuilt = BinaryTree::<Blake3Hasher>::from_sorted_entries(model.clone()).unwrap();
        prop_assert_eq!(tree.root(), rebuilt.root());
        prop_assert_eq!(tree.entries().map(|(key, value)| (key.to_vec(), value)).collect::<BTreeMap<_, _>>(), model);
        prop_assert_eq!(snapshot.root(), before);
    }
    #[test]
    fn mutations_match_sorted_rebuild(operations in collection::vec((any::<[u8; 4]>(), proptest::option::of(any::<[u8; 32]>())), 0..100)) {
        let mut tree = tree();
        let mut model = BTreeMap::new();
        for (key, value) in operations {
            let snapshot = tree.clone();
            let before = snapshot.root();
            if let Some(value) = value {
                let value = B256::from(value);
                prop_assert_eq!(tree.insert(key.to_vec(), value).unwrap(), model.insert(key.to_vec(), value));
            } else {
                prop_assert_eq!(tree.remove(&key), model.remove(key.as_slice()));
            }
            prop_assert_eq!(snapshot.root(), before);
            let rebuilt = BinaryTree::<Blake3Hasher>::from_sorted_entries(model.iter().map(|(key, value)| (key.clone(), *value))).unwrap();
            prop_assert_eq!(tree.root(), rebuilt.root());
            prop_assert_eq!(tree.len(), model.len());
            prop_assert_eq!(tree.get(&key), model.get(key.as_slice()).copied());
            prop_assert_eq!(tree.proof(&key).verify::<Blake3Hasher>(tree.root(), &key).unwrap(), model.get(key.as_slice()).copied());
        }
    }

    #[test]
    fn deleting_every_leaf_restores_empty(entries in collection::btree_map(any::<[u8; 2]>(), any::<[u8; 32]>(), 0..100)) {
        let mut tree = tree();
        for (key, value) in &entries { tree.insert(key.to_vec(), B256::from(*value)).unwrap(); }
        for (key, value) in entries { prop_assert_eq!(tree.remove(&key), Some(B256::from(value))); }
        prop_assert!(tree.is_empty());
        prop_assert_eq!(tree.root(), B256::ZERO);
    }

    #[test]
    fn repeated_variable_length_mutations_match_model(
        operations in collection::vec(
            (collection::vec(0u8..4, 1..4), proptest::option::of(any::<[u8; 32]>())),
            0..100,
        ),
    ) {
        // The small key space deliberately repeats keys and generates proper-prefix conflicts.
        let mut tree = tree();
        let mut model = BTreeMap::<Vec<u8>, B256>::new();
        for (key, value) in operations {
            let snapshot = tree.clone();
            let before = snapshot.root();
            let before_entries = model.clone();
            if let Some(value) = value {
                let value = B256::from(value);
                let conflict = model.keys().any(|stored| {
                    stored != &key && (stored.starts_with(&key) || key.starts_with(stored))
                });
                if conflict {
                    prop_assert_eq!(tree.insert(key.clone(), value), Err(TreeError::PrefixConflict));
                    prop_assert_eq!(tree.root(), before);
                } else {
                    prop_assert_eq!(tree.insert(key.clone(), value).unwrap(), model.insert(key.clone(), value));
                }
            } else {
                prop_assert_eq!(tree.remove(&key), model.remove(&key));
            }
            prop_assert_eq!(snapshot.root(), before);
            prop_assert_eq!(
                snapshot.entries().map(|(key, value)| (key.to_vec(), value)).collect::<BTreeMap<_, _>>(),
                before_entries,
            );
            let rebuilt = BinaryTree::<Blake3Hasher>::from_sorted_entries(
                model.iter().map(|(key, value)| (key.clone(), *value)),
            ).unwrap();
            prop_assert_eq!(tree.root(), rebuilt.root());
            prop_assert_eq!(tree.len(), model.len());
            prop_assert_eq!(tree.get(&key), model.get(&key).copied());
            prop_assert_eq!(tree.proof(&key).verify::<Blake3Hasher>(tree.root(), &key).unwrap(), model.get(&key).copied());
            for (stored, value) in &model {
                prop_assert_eq!(tree.proof(stored).verify::<Blake3Hasher>(tree.root(), stored).unwrap(), Some(*value));
            }
        }
    }
}
