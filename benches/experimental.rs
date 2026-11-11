//! Same-key workloads comparing explicit experimental APIs with the default tree.
//! Arena history is compacted outside timed mutation regions to bound retained nodes.

use alloy_primitives::{B256, U256, keccak256};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use pbt::{BinaryTree, Blake3Hasher, experimental::ArenaTree};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

type Tree = BinaryTree<Blake3Hasher>;
type Arena = ArenaTree<Blake3Hasher>;

fn entries(size: usize) -> Vec<(Vec<u8>, B256)> {
    let mut entries: Vec<_> = (0..size)
        .map(|index| {
            (keccak256((index as u64).to_be_bytes()).to_vec(), B256::from(U256::from(index + 1)))
        })
        .collect();
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("experimental/build");
    group.sample_size(10);
    for size in [1024, 16384, 65536] {
        let entries = entries(size);
        let root = Tree::from_sorted_entries(entries.clone()).unwrap().root();
        assert_eq!(Tree::from_sorted_entries_parallel(entries.clone()).unwrap().root(), root);
        assert_eq!(Arena::from_sorted_entries(entries.clone()).unwrap().root(), root);
        group.bench_function(BenchmarkId::new("serial", size), |b| {
            b.iter_batched(
                || entries.clone(),
                |entries| black_box(Tree::from_sorted_entries(entries).unwrap()),
                BatchSize::LargeInput,
            );
        });
        group.bench_function(BenchmarkId::new("parallel", size), |b| {
            b.iter_batched(
                || entries.clone(),
                |entries| black_box(Tree::from_sorted_entries_parallel(entries).unwrap()),
                BatchSize::LargeInput,
            );
        });
        group.bench_function(BenchmarkId::new("arena", size), |b| {
            b.iter_batched(
                || entries.clone(),
                |entries| black_box(Arena::from_sorted_entries(entries).unwrap()),
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("experimental/operations");
    group.sample_size(10);
    for size in [1024, 16384] {
        let entries = entries(size);
        let key = entries[size / 2].0.clone();
        let tree = Tree::from_sorted_entries(entries.clone()).unwrap();
        let mut arena = Arena::from_sorted_entries(entries).unwrap();
        group.bench_function(BenchmarkId::new("reference_get", size), |b| {
            b.iter(|| black_box(tree.get(black_box(&key))));
        });
        group.bench_function(BenchmarkId::new("arena_get", size), |b| {
            b.iter(|| black_box(arena.get(black_box(&key)).unwrap()));
        });
        group.bench_function(BenchmarkId::new("reference_replace", size), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let mut fork = tree.clone();
                    let owned_key = key.clone();
                    let start = Instant::now();
                    fork.insert(owned_key, black_box(B256::repeat_byte(0xa5))).unwrap();
                    elapsed += start.elapsed();
                    black_box(fork.root());
                    drop(fork);
                }
                elapsed
            });
        });
        group.bench_function(BenchmarkId::new("arena_replace", size), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for iteration in 0..iterations {
                    let mut fork = arena.clone();
                    let owned_key = key.clone();
                    let start = Instant::now();
                    fork.insert(owned_key, black_box(B256::repeat_byte(0xa5))).unwrap();
                    elapsed += start.elapsed();
                    black_box(fork.root());
                    drop(fork);
                    if iteration % 1024 == 1023 || iteration + 1 == iterations {
                        arena.compact().unwrap();
                    }
                }
                elapsed
            });
        });
    }
    group.finish();
}

criterion_group!(benches, construction, operations);
criterion_main!(benches);
