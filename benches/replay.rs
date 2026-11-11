//! Authenticated BAL replay and finality pruning; parent states and graph construction are untimed.

use alloy_eip7928::{
    AccountChanges, BalanceChange, BlockAccessIndex, SlotChanges, StorageChange,
    compute_block_access_list_hash,
};
use alloy_primitives::{Address, U256, keccak256};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pbt::{
    AccountData, Blake3Hasher, StateTree,
    replay::{ReplayChain, replay},
};
use std::{hint::black_box, num::NonZeroUsize};

type State = StateTree<Blake3Hasher>;

fn address(index: usize) -> Address {
    let mut bytes = [0; 20];
    bytes[12..].copy_from_slice(&(index as u64).to_be_bytes());
    Address::from(bytes)
}

fn replay_blocks(c: &mut Criterion) {
    let mut parent = State::new();
    for index in 0..16_384 {
        parent
            .set_account(
                address(index),
                AccountData { nonce: 1, balance: U256::from(100), ..Default::default() },
            )
            .unwrap();
    }
    let mut group = c.benchmark_group("replay/authenticated_bal");
    for count in [1, 64, 256, 4_096] {
        let bal: Vec<_> = (0..count)
            .map(|index| {
                let mut change = AccountChanges::new(address(index));
                change
                    .balance_changes
                    .push(BalanceChange::new(BlockAccessIndex::new(1), U256::from(101)));
                for slot in [0, 63, 64, 256] {
                    change.storage_changes.push(SlotChanges::new(
                        U256::from(slot),
                        vec![StorageChange::new(BlockAccessIndex::new(1), U256::from(1))],
                    ));
                }
                change
            })
            .collect();
        let hash = compute_block_access_list_hash(&bal);
        assert!(replay(&parent, &bal, hash).is_ok());
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter_batched(
                || (),
                |()| {
                    black_box(replay(black_box(&parent), black_box(&bal), black_box(hash)).unwrap())
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn finality(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay/finalize_linear_chain");
    let bal_hash = compute_block_access_list_hash(&[]);
    for count in [128, 512, 1_024, 4_096, 16_384] {
        let hashes: Vec<_> =
            (0..count).map(|index: usize| keccak256((index as u64).to_be_bytes())).collect();
        let mut chain = ReplayChain::<Blake3Hasher>::with_limit(
            hashes[0],
            State::new(),
            NonZeroUsize::new(count).unwrap(),
        );
        for pair in hashes.windows(2) {
            chain.insert(pair[1], pair[0], &[], bal_hash).unwrap();
        }
        group.throughput(Throughput::Elements(count as u64));
        for (name, target) in [
            ("same_anchor", hashes[0]),
            ("midpoint", hashes[count / 2]),
            ("tip", hashes[count - 1]),
        ] {
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter_batched(
                    || chain.clone(),
                    |mut chain| {
                        let state = chain.finalize(black_box(target)).unwrap();
                        black_box((chain, state))
                    },
                    BatchSize::LargeInput,
                );
            });
        }
    }
    group.finish();
}

criterion_group!(benches, replay_blocks, finality);
criterion_main!(benches);
