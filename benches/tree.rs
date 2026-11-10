//! Core tree costs with deterministic Ethereum header keys and untimed setup.

use alloy_primitives::{Address, B256};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pbt::{BinaryTree, Blake3Hasher, key};
use std::hint::black_box;

type Tree = BinaryTree<Blake3Hasher>;
const SIZES: [usize; 2] = [1_024, 16_384];

fn tree_key(index: usize) -> Vec<u8> {
    let mut address = [0; 20];
    address[12..].copy_from_slice(&(index as u64).to_be_bytes());
    key::header_key::<Blake3Hasher>(Address::from(address), key::BASIC_DATA_LEAF_KEY).to_vec()
}

fn entries(size: usize) -> Vec<(Vec<u8>, B256)> {
    (0..size)
        .map(|index| {
            let mut value = [0; 32];
            value[24..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
            (tree_key(index), B256::from(value))
        })
        .collect()
}

fn populated_tree(size: usize) -> Tree {
    let mut entries = entries(size);
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Tree::from_sorted_entries(entries).unwrap()
}

fn build(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree/build");
    for size in SIZES {
        let unsorted = entries(size);
        let mut sorted = unsorted.clone();
        sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("sorted", size), &sorted, |b, entries| {
            b.iter_batched(
                || entries.clone(),
                |entries| black_box(Tree::from_sorted_entries(black_box(entries)).unwrap()),
                BatchSize::LargeInput,
            );
        });
        group.bench_with_input(
            BenchmarkId::new("incremental_insert", size),
            &unsorted,
            |b, entries| {
                b.iter_batched(
                    || entries.clone(),
                    |entries| {
                        let mut tree = Tree::new();
                        for (key, value) in black_box(entries) {
                            tree.insert(key, value).unwrap();
                        }
                        black_box(tree)
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

fn mutate(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree/mutate");
    group.throughput(Throughput::Elements(1));
    for size in SIZES {
        let tree = populated_tree(size);
        let key = tree_key(size / 2);
        group.bench_function(BenchmarkId::new("replace", size), |b| {
            b.iter_batched(
                || (tree.clone(), key.clone()),
                |(mut tree, key)| {
                    let previous =
                        tree.insert(black_box(key), black_box(B256::repeat_byte(0xa5))).unwrap();
                    black_box((tree, previous))
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function(BenchmarkId::new("remove", size), |b| {
            b.iter_batched(
                || tree.clone(),
                |mut tree| {
                    let previous = tree.remove(black_box(&key));
                    black_box((tree, previous))
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn proofs(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree/proof");
    group.throughput(Throughput::Elements(1));
    for size in SIZES {
        let tree = populated_tree(size);
        let root = tree.root();
        for (kind, key) in [("membership", tree_key(size / 2)), ("absence", tree_key(size))] {
            let proof = tree.proof(&key);
            let expected = tree.get(&key);
            assert_eq!(proof.verify::<Blake3Hasher>(root, &key).unwrap(), expected);
            group.bench_function(BenchmarkId::new(format!("generate_{kind}"), size), |b| {
                b.iter(|| black_box(tree.proof(black_box(&key))));
            });
            group.bench_function(BenchmarkId::new(format!("verify_{kind}"), size), |b| {
                b.iter(|| {
                    black_box(
                        black_box(&proof)
                            .verify::<Blake3Hasher>(black_box(root), black_box(&key))
                            .unwrap(),
                    )
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, build, mutate, proofs);
criterion_main!(benches);
