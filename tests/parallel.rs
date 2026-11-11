//! Dense parallel updates agree with incremental and serial batch commitments.
#![cfg(feature = "parallel")]

use alloy_primitives::{Address, B256, U256};
use pbt::{AccountData, BinaryTree, Blake3Hasher, StateTree, StateUpdate, TreeError};

#[cfg(feature = "bal")]
use alloy_eip7928::{
    AccountChanges, BalanceChange, BlockAccessIndex, compute_block_access_list_hash,
};

#[test]
fn sparse_and_dense_parallel_batches_preserve_serial_roots_and_snapshots() {
    let entries: Vec<_> = (0u64..8192)
        .map(|index| (index.to_be_bytes().to_vec(), B256::from(U256::from(index + 1))))
        .collect();
    let anchor = BinaryTree::<Blake3Hasher>::from_sorted_entries(entries).unwrap();
    let before = anchor.root();
    for threads in [1, 4] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        pool.install(|| {
            for count in [1, 64, 4096, 8192] {
                let changes: Vec<_> = (0u64..count)
                    .map(|index| {
                        (index.to_be_bytes().to_vec(), (index % 3 != 0).then_some(B256::ZERO))
                    })
                    .collect();
                let mut serial = anchor.clone();
                let mut parallel = anchor.clone();
                serial.apply_updates(changes.clone()).unwrap();
                parallel.apply_updates_parallel(changes).unwrap();
                assert_eq!(parallel.root(), serial.root());
                assert_eq!(
                    parallel.entries().collect::<Vec<_>>(),
                    serial.entries().collect::<Vec<_>>()
                );
                assert_eq!(anchor.root(), before);
            }
        });
    }
    let mut tree = BinaryTree::<Blake3Hasher>::new();
    tree.insert(vec![1], B256::ZERO).unwrap();
    let root = tree.root();
    assert_eq!(
        tree.apply_updates_parallel([(vec![1, 2], Some(B256::ZERO))]),
        Err(TreeError::PrefixConflict)
    );
    assert_eq!(tree.root(), root);
}

#[test]
fn dense_state_batches_and_parallel_replay_match_serial_state() {
    let changes: Vec<_> = (0u64..4096)
        .map(|index| StateUpdate::Account {
            address: Address::from_word(B256::from(U256::from(index))),
            account: Some(AccountData { nonce: 1, balance: U256::from(1), ..Default::default() }),
        })
        .collect();
    let mut serial = StateTree::<Blake3Hasher>::new();
    let mut parallel = serial.clone();
    serial.apply_batch(changes.clone()).unwrap();
    parallel.apply_batch_parallel(changes).unwrap();
    assert_eq!(parallel.root(), serial.root());
    #[cfg(feature = "bal")]
    {
        let bal: Vec<_> = serial
            .accounts()
            .map(|(address, _)| {
                let mut change = AccountChanges::new(address);
                change
                    .balance_changes
                    .push(BalanceChange::new(BlockAccessIndex::new(1), U256::from(2)));
                change
            })
            .collect();
        let hash = compute_block_access_list_hash(&bal);
        let serial_root = pbt::replay::replay(&serial, &bal, hash).unwrap().root();
        assert_eq!(
            pbt::replay::replay_parallel(&parallel, &bal, hash).unwrap().root(),
            serial_root
        );
        assert!(matches!(
            pbt::replay::replay_parallel(&parallel, &bal, B256::ZERO),
            Err(pbt::replay::ReplayError::BalHashMismatch)
        ));
        assert_eq!(parallel.root(), serial.root());
    }
}
