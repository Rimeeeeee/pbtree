//! Synchronous durable saves and authenticated reconstruction of an uncached StateTree.
//! RocksDB and OS caches may remain warm; fixture setup, mutation, and collection are untimed.

use alloy_primitives::{Address, B256, U256};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pbt::{
    AccountData, Blake3Hasher, StateTree, migration::DecodeLimits, persistence::NodeDatabase,
};
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

type State = StateTree<Blake3Hasher>;
const SIZES: [usize; 2] = [256, 4_096];

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
    group.sample_size(10);
    group.throughput(Throughput::Elements(1));
    for size in SIZES {
        let mut state = populated_state(size);
        let fixture = Fixture::new(&state);
        let database = &fixture.database;
        let _mutation = database.mutation.lock().unwrap();
        let target = address(size / 2);
        group.bench_function(BenchmarkId::new("sync_one_account_change", size), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for iteration in 0..iterations {
                    let previous_root = state.root();
                    let mut account = state.account(target).unwrap().clone();
                    account.nonce = account.nonce.checked_add(1).unwrap();
                    state.set_account(target, account).unwrap();
                    assert_ne!(state.root(), previous_root);
                    assert_eq!(database.leaf_count(state.root()).unwrap(), None);

                    let start = Instant::now();
                    database.save(black_box(&state), black_box(limits)).unwrap();
                    elapsed += start.elapsed();

                    database.remove_root(previous_root).unwrap();
                    // Bound unreachable paths without charging GC or root removal to save.
                    if iteration % 64 == 63 || iteration + 1 == iterations {
                        database.collect::<Blake3Hasher>().unwrap();
                    }
                }
                elapsed
            });
        });
        group.bench_function(BenchmarkId::new("sync_unchanged", size), |b| {
            b.iter(|| database.save(black_box(&state), black_box(limits)).unwrap());
        });
    }
    group.finish();
}

fn loads(c: &mut Criterion) {
    let limits = DecodeLimits::default();
    let mut group = c.benchmark_group("persistence/load");
    group.sample_size(10);
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

criterion_group!(benches, saves, loads);
criterion_main!(benches);
