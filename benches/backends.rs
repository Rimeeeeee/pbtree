//! Identical-input sorted construction for the serial and parallel builders.

use alloy_primitives::{B256, U256, keccak256};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use pbt::{BinaryTree, Blake3Hasher};
use std::hint::black_box;

type Tree = BinaryTree<Blake3Hasher>;

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
    let mut group = c.benchmark_group("backends/build");
    for size in [16_384, 65_536, 262_144] {
        let entries = entries(size);
        let root = Tree::from_sorted_entries(entries.clone()).unwrap().root();
        assert_eq!(Tree::from_sorted_entries_parallel(entries.clone()).unwrap().root(), root);
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
    }
    group.finish();
}

criterion_group!(benches, construction);
criterion_main!(benches);
