//! Differential checks for opt-in arena snapshots and parallel construction.
#![cfg(feature = "experimental")]

use alloy_primitives::B256;
use pbt::{
    BinaryTree, Blake3Hasher, TreeError,
    experimental::{ArenaError, ArenaTree},
};
use proptest::{collection, prelude::*};
use std::collections::BTreeMap;

#[test]
fn parallel_builders_and_arena_match_serial_in_single_and_multiple_thread_pools() {
    let entries: Vec<_> = (0u64..8192)
        .map(|index| {
            (index.to_be_bytes().to_vec(), B256::from(alloy_primitives::U256::from(index)))
        })
        .collect();
    let serial = BinaryTree::<Blake3Hasher>::from_sorted_entries(entries.clone()).unwrap();
    let arena = ArenaTree::<Blake3Hasher>::from_sorted_entries(entries.clone()).unwrap();
    assert_eq!(arena.root(), serial.root());
    assert_eq!(arena.allocated_nodes().unwrap(), 2 * entries.len() - 1);
    for threads in [1, 4] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let parallel = pool.install(|| {
            BinaryTree::<Blake3Hasher>::from_sorted_entries_parallel(entries.clone()).unwrap()
        });
        assert_eq!(parallel.root(), serial.root());
    }
    let key = 4000u64.to_be_bytes();
    assert_eq!(
        arena.proof(&key).unwrap().verify::<Blake3Hasher>(serial.root(), &key).unwrap(),
        serial.get(&key)
    );
}

#[test]
fn concurrent_forks_and_compaction_preserve_retained_roots() {
    let mut anchor = ArenaTree::<Blake3Hasher>::new();
    anchor.insert(vec![0, 0], B256::repeat_byte(1)).unwrap();
    let root = anchor.root();
    let workers: Vec<_> = [1u8, 2]
        .into_iter()
        .map(|prefix| {
            let mut state = anchor.clone();
            std::thread::spawn(move || {
                for key in 0..100u8 {
                    state.insert(vec![prefix, key], B256::repeat_byte(key)).unwrap();
                }
                state
            })
        })
        .collect();
    for (prefix, worker) in [1u8, 2].into_iter().zip(workers) {
        let mut fork = worker.join().unwrap();
        let mut expected = BinaryTree::<Blake3Hasher>::new();
        expected.insert(vec![0, 0], B256::repeat_byte(1)).unwrap();
        for key in 0..100u8 {
            expected.insert(vec![prefix, key], B256::repeat_byte(key)).unwrap();
        }
        assert_eq!(fork.root(), expected.root());
        fork.compact().unwrap();
        assert_eq!(fork.allocated_nodes().unwrap(), fork.len() * 2 - 1);
        assert_eq!(fork.root(), expected.root());
    }
    assert_eq!(anchor.root(), root);
    assert_eq!(anchor.get(&[1, 0]).unwrap(), None);
}

#[test]
fn maximum_prefix_merges_match_the_default_tree() {
    let mut arena = ArenaTree::<Blake3Hasher>::new();
    let mut reference = BinaryTree::<Blake3Hasher>::new();
    let keys: Vec<_> = [0, 1, 128]
        .into_iter()
        .map(|last| {
            let mut key = vec![0; pbt::MAX_KEY_LENGTH];
            *key.last_mut().unwrap() = last;
            key
        })
        .collect();
    for key in &keys {
        arena.insert(key.clone(), B256::repeat_byte(1)).unwrap();
        reference.insert(key.clone(), B256::repeat_byte(1)).unwrap();
    }
    arena.remove(&keys[2]).unwrap();
    reference.remove(&keys[2]);
    assert_eq!(arena.root(), reference.root());
    assert_eq!(
        arena.proof(&keys[0]).unwrap().verify::<Blake3Hasher>(arena.root(), &keys[0]).unwrap(),
        Some(B256::repeat_byte(1))
    );
    arena.compact().unwrap();
    assert_eq!(arena.root(), reference.root());
}

#[test]
fn parallel_comb_build_remains_bounded_on_small_worker_stacks() {
    let depth = 8192usize;
    let zero = vec![0; depth / 8];
    let entries: Vec<_> = std::iter::once((zero.clone(), B256::ZERO))
        .chain((0..depth).rev().map(|index| {
            let mut key = zero.clone();
            key[index / 8] |= 0x80 >> (index % 8);
            (key, B256::repeat_byte(1))
        }))
        .collect();
    let serial = BinaryTree::<Blake3Hasher>::from_sorted_entries(entries.clone()).unwrap();
    let pool =
        rayon::ThreadPoolBuilder::new().num_threads(4).stack_size(64 * 1024).build().unwrap();
    let parallel =
        pool.install(|| BinaryTree::<Blake3Hasher>::from_sorted_entries_parallel(entries).unwrap());
    assert_eq!(serial.root(), parallel.root());
}

proptest! {
    #[test]
    fn arena_mutations_proofs_and_snapshots_match_reference(
        operations in collection::vec((collection::vec(0u8..8, 1..5), proptest::option::of(any::<[u8; 32]>())), 0..100),
    ) {
        let mut arena = ArenaTree::<Blake3Hasher>::new();
        let mut reference = BinaryTree::<Blake3Hasher>::new();
        for (index, (key, value)) in operations.into_iter().enumerate() {
            let snapshot = arena.clone();
            let before = reference.entries().map(|(key, value)| (key.to_vec(), value)).collect::<BTreeMap<_, _>>();
            if let Some(value) = value {
                let value = B256::from(value);
                match reference.insert(key.clone(), value) {
                    Ok(previous) => prop_assert_eq!(arena.insert(key.clone(), value).unwrap(), previous),
                    Err(TreeError::PrefixConflict) => prop_assert!(matches!(arena.insert(key.clone(), value), Err(ArenaError::Tree(TreeError::PrefixConflict)))),
                    Err(error) => panic!("unexpected reference error: {error}"),
                }
            } else {
                prop_assert_eq!(arena.remove(&key).unwrap(), reference.remove(&key));
            }
            if index % 17 == 0 { arena.compact().unwrap(); }
            prop_assert_eq!(arena.root(), reference.root());
            prop_assert_eq!(arena.len(), reference.len());
            prop_assert_eq!(arena.get(&key).unwrap(), reference.get(&key));
            prop_assert_eq!(arena.proof(&key).unwrap().verify::<Blake3Hasher>(reference.root(), &key).unwrap(), reference.get(&key));
            prop_assert_eq!(snapshot.entries().unwrap().into_iter().collect::<BTreeMap<_, _>>(), before);
            prop_assert_eq!(arena.entries().unwrap(), reference.entries().map(|(key, value)| (key.to_vec(), value)).collect::<Vec<_>>());
        }
    }
}
