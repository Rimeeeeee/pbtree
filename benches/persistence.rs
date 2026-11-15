//! Synchronous durable saves and authenticated reconstruction of an uncached StateTree.
//! RocksDB and OS caches may remain warm; fixture setup, mutation, and collection are untimed.

#![allow(missing_docs)] // CodSpeed's macros generate undocumented public functions.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pbt::{
    AccountData, Blake3Hasher, StateTree, StateUpdate,
    migration::DecodeLimits,
    persistence::{AccountUpdate, CodeUpdate, NodeDatabase, SubtreeCache},
};
use std::{hint::black_box, sync::Arc};

type State = StateTree<Blake3Hasher>;
const SIZES: [usize; 3] = [256, 4_096, 16_384];

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

struct Fixture {
    // Close RocksDB before removing its directory.
    database: Arc<NodeDatabase>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new(state: &State) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = NodeDatabase::open(directory.path()).unwrap();
        {
            let _mutation = database.mutation.lock().unwrap();
            database.save(state, DecodeLimits::default()).unwrap();
        }
        Self { database, _directory: directory }
    }
}

fn saves(c: &mut Criterion) {
    let limits = DecodeLimits::default();
    let mut group = c.benchmark_group("persistence/save");
    group.throughput(Throughput::Elements(1));
    for size in SIZES {
        let mut state = populated_state(size);
        let fixture = Fixture::new(&state);
        let database = &fixture.database;
        let _mutation = database.mutation.lock().unwrap();
        let target = address(size / 2);
        let mut previous_root = None;
        let mut saves = 0u64;
        group.bench_function(BenchmarkId::new("sync_one_account_change", size), |b| {
            // Prepare exactly one new root before each measured save. Removing the
            // previous root and collecting unreachable paths happen between saves.
            b.iter_batched_ref(
                || {
                    if let Some(root) = previous_root.take() {
                        database.remove_root(root).unwrap();
                        saves += 1;
                        if saves.is_multiple_of(64) {
                            database.collect::<Blake3Hasher>().unwrap();
                        }
                    }
                    let root = state.root();
                    let mut account = state.account(target).unwrap().clone();
                    account.nonce = account.nonce.checked_add(1).unwrap();
                    state.set_account(target, account).unwrap();
                    assert_ne!(state.root(), root);
                    assert_eq!(database.leaf_count(state.root()).unwrap(), None);
                    previous_root = Some(root);
                    state.clone()
                },
                |state| database.save(black_box(&*state), black_box(limits)).unwrap(),
                BatchSize::PerIteration,
            );
        });
        if let Some(root) = previous_root {
            database.remove_root(root).unwrap();
            database.collect::<Blake3Hasher>().unwrap();
        }
        group.bench_function(BenchmarkId::new("sync_unchanged", size), |b| {
            b.iter(|| database.save(black_box(&state), black_box(limits)).unwrap());
        });
    }
    group.finish();
}

fn loads(c: &mut Criterion) {
    let limits = DecodeLimits::default();
    let mut group = c.benchmark_group("persistence/load");
    for size in SIZES {
        let state = populated_state(size);
        let root = state.root();
        let count = state.tree().len();
        let fixture = Fixture::new(&state);
        drop(state);
        let database = &fixture.database;
        let _mutation = database.mutation.lock().unwrap();
        let restored = database.load::<Blake3Hasher>(root, count, limits).unwrap();
        assert_eq!(restored.root(), root);
        assert_eq!(restored.accounts().count(), size);
        drop(restored);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_function(BenchmarkId::new("verified_cold_state_warm_db_cache", size), |b| {
            b.iter_batched(
                || (),
                |()| {
                    black_box(
                        database
                            .load::<Blake3Hasher>(
                                black_box(root),
                                black_box(count),
                                black_box(limits),
                            )
                            .unwrap(),
                    )
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn proofs(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistence/proof");
    group.throughput(Throughput::Elements(1));
    for size in SIZES {
        let state = populated_state(size);
        let root = state.root();
        let key = pbt::key::storage_key::<Blake3Hasher>(address(size / 2), U256::from(256));
        let expected = state.tree().proof(&key);
        let fixture = Fixture::new(&state);
        drop(state);
        let database = &fixture.database;
        let _mutation = database.mutation.lock().unwrap();
        assert_eq!(database.proof::<Blake3Hasher>(root, &key).unwrap(), expected);
        group.bench_function(BenchmarkId::new("native_path_warm_db_cache", size), |b| {
            b.iter(|| database.proof::<Blake3Hasher>(black_box(root), black_box(&key)).unwrap());
        });
    }
    group.finish();
}

fn execution(c: &mut Criterion) {
    let limits = DecodeLimits::native();
    let mut group = c.benchmark_group("persistence/execution");
    group.throughput(Throughput::Elements(64));
    for size in [4_096, 16_384] {
        let state = populated_state(size);
        let root = state.root();
        let leaves = state.tree().len();
        let fixture = Fixture::new(&state);
        let db = &fixture.database;

        let targets: Vec<_> = (0..64).map(|index| address(index * size / 64)).collect();
        let slot = U256::from(64);
        let value = B256::repeat_byte(2);
        let code_hash = keccak256([0x60; 64]);
        let apply_eager = |state: &mut State| {
            state
                .apply_batch(targets.iter().flat_map(|&address| {
                    [
                        StateUpdate::Account {
                            address,
                            account: Some(AccountData {
                                nonce: 2,
                                balance: U256::from(100),
                                code: Bytes::from_static(&[0x60; 64]),
                            }),
                        },
                        StateUpdate::Storage { address, slot, value },
                    ]
                }))
                .unwrap();
        };
        let apply_disk = |cache: &Arc<SubtreeCache<Blake3Hasher>>| {
            let checkpoint = {
                let _mutation = db.mutation.lock().unwrap();
                cache.checkpoint(root).unwrap()
            };
            let mut state = checkpoint.overlay();
            for &address in &targets {
                state
                    .update_account(
                        address,
                        Some(AccountUpdate {
                            nonce: 2,
                            balance: U256::from(100),
                            code: CodeUpdate::Retain(code_hash),
                        }),
                        false,
                        [(slot, value)],
                    )
                    .unwrap();
            }
            state
        };
        let cache = Arc::new(SubtreeCache::new(db.clone(), 16_384));
        let mut expected = state.clone();
        apply_eager(&mut expected);
        assert_eq!(apply_disk(&cache).root(), expected.root());
        drop(expected);

        group.bench_function(BenchmarkId::new("cold_parent_eager", size), |b| {
            b.iter_batched(
                || (),
                |()| {
                    let mut restored = db.load::<Blake3Hasher>(root, leaves, limits).unwrap();
                    apply_eager(&mut restored);
                    black_box(restored)
                },
                BatchSize::PerIteration,
            );
        });
        group.bench_function(BenchmarkId::new("cold_parent_overlay", size), |b| {
            b.iter_batched(
                || Arc::new(SubtreeCache::new(db.clone(), 16_384)),
                |cache| black_box(apply_disk(&cache)),
                BatchSize::PerIteration,
            );
        });
        group.bench_function(BenchmarkId::new("warm_parent_overlay", size), |b| {
            b.iter_batched(|| (), |()| black_box(apply_disk(&cache)), BatchSize::PerIteration);
        });
        group.bench_function(BenchmarkId::new("warm_parent_eager", size), |b| {
            b.iter_batched(
                || state.clone(),
                |mut state| {
                    apply_eager(&mut state);
                    black_box(state)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, saves, loads, proofs, execution);
criterion_main!(benches);
