//! Persistent Ethereum-state updates; fixture construction and snapshot cloning are untimed.

#![allow(missing_docs)] // CodSpeed's macros generate undocumented public functions.

use alloy_primitives::{Address, B256, U256};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pbt::{AccountData, Blake3Hasher, StateTree};
use std::hint::black_box;

type State = StateTree<Blake3Hasher>;
const SIZES: [usize; 3] = [128, 2_048, 16_384];

fn address(index: usize) -> Address {
    let mut bytes = [0; 20];
    bytes[12..].copy_from_slice(&(index as u64).to_be_bytes());
    Address::from(bytes)
}

fn populated_state(size: usize) -> State {
    let mut state = State::new();
    for index in 0..size {
        let address = address(index);
        state
            .set_account(
                address,
                AccountData { nonce: 1, balance: U256::from(100), code: vec![0x60; 64].into() },
            )
            .unwrap();
        for slot in [0, 63, 64, 256] {
            state.set_storage(address, U256::from(slot), B256::repeat_byte(1)).unwrap();
        }
    }
    state
}

fn updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("state/update");
    group.throughput(Throughput::Elements(1));
    for size in SIZES {
        let state = populated_state(size);
        let target = address(size / 2);
        let mut replacement = state.account(target).unwrap().clone();
        replacement.balance += U256::from(1);
        group.bench_function(BenchmarkId::new("account_balance", size), |b| {
            b.iter_batched(
                || (state.clone(), replacement.clone()),
                |(mut state, account)| {
                    state.set_account(black_box(target), black_box(account)).unwrap();
                    black_box(state)
                },
                BatchSize::SmallInput,
            );
        });
        for (name, slot, value) in [
            ("inline_storage_replace", 63, B256::repeat_byte(2)),
            ("overflow_storage_replace", 256, B256::repeat_byte(2)),
            ("overflow_storage_delete", 256, B256::ZERO),
        ] {
            let slot = U256::from(slot);
            group.bench_function(BenchmarkId::new(name, size), |b| {
                b.iter_batched(
                    || state.clone(),
                    |mut state| {
                        state
                            .set_storage(black_box(target), black_box(slot), black_box(value))
                            .unwrap();
                        black_box(state)
                    },
                    BatchSize::SmallInput,
                );
            });
        }
        group.bench_function(BenchmarkId::new("remove_account_shared_code", size), |b| {
            b.iter_batched(
                || state.clone(),
                |mut state| {
                    black_box(state.remove_account(black_box(target)));
                    black_box(state)
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, updates);
criterion_main!(benches);
