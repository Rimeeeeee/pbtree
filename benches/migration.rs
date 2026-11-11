//! Canonical artifact conversion with authenticated roots prepared outside the timed region.

use alloy_primitives::{Address, B256, U256};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pbt::{
    AccountData, Blake3Hasher, StateTree,
    migration::{Artifacts, DecodeLimits, mpt_root},
};
use std::hint::black_box;

type State = StateTree<Blake3Hasher>;

fn populated_state(size: usize) -> State {
    let mut state = State::new();
    for index in 0..size {
        let mut bytes = [0; 20];
        bytes[12..].copy_from_slice(&(index as u64).to_be_bytes());
        let address = Address::from(bytes);
        let code = match index % 3 {
            0 => Vec::new(),
            1 => vec![0x60; 65],
            _ => [vec![0xef, 1, 0], Address::repeat_byte(0x42).to_vec()].concat(),
        };
        state
            .set_account(
                address,
                AccountData { nonce: 1, balance: U256::from(100), code: code.into() },
            )
            .unwrap();
        for slot in [0, 63, 64, 256] {
            state.set_storage(address, U256::from(slot), B256::repeat_byte(1)).unwrap();
        }
    }
    state
}

fn artifacts(c: &mut Criterion) {
    let mut group = c.benchmark_group("migration/artifacts");
    for size in [128, 2_048, 16_384] {
        let state = populated_state(size);
        let encoded = Artifacts::from_state(&state).unwrap();
        let anchor = mpt_root(&state);
        let root = state.root();
        let limits = DecodeLimits::default();
        assert_eq!(encoded.verify::<Blake3Hasher>(anchor, limits).unwrap().root(), root);
        assert_eq!(encoded.restore::<Blake3Hasher>(root, limits).unwrap().root(), root);
        group.throughput(Throughput::Bytes(
            (encoded.snapshot.len() + encoded.preimages.len()) as u64,
        ));
        group.bench_function(BenchmarkId::new("encode", size), |b| {
            b.iter_batched(
                || (),
                |()| black_box(Artifacts::from_state(black_box(&state)).unwrap()),
                BatchSize::LargeInput,
            );
        });
        group.bench_function(BenchmarkId::new("verify_mpt_anchor", size), |b| {
            b.iter_batched(
                || (),
                |()| {
                    black_box(
                        encoded
                            .verify::<Blake3Hasher>(black_box(anchor), black_box(limits))
                            .unwrap(),
                    )
                },
                BatchSize::LargeInput,
            );
        });
        group.bench_function(BenchmarkId::new("restore_pbt_checkpoint", size), |b| {
            b.iter_batched(
                || (),
                |()| {
                    black_box(
                        encoded
                            .restore::<Blake3Hasher>(black_box(root), black_box(limits))
                            .unwrap(),
                    )
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, artifacts);
criterion_main!(benches);
