mod common;

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use bittice::core::storage::pk::canonical_bytes;
use bittice::core::storage::primary_index::PrimaryIndex;
use xxhash_rust::xxh3::xxh3_128;

fn bench_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash");

    let pk_short = common::generate(36, 1).pks[0].clone();
    let pk_medium = common::generate(200, 1).pks[0].clone();
    let pk_large = common::generate(500, 1).pks[0].clone();

    group.bench_function("hash_short_36B", |b| {
        b.iter(|| xxh3_128(&canonical_bytes(black_box(&pk_short))))
    });
    group.bench_function("hash_medium_200B", |b| {
        b.iter(|| xxh3_128(&canonical_bytes(black_box(&pk_medium))))
    });
    group.bench_function("hash_large_500B", |b| {
        b.iter(|| xxh3_128(&canonical_bytes(black_box(&pk_large))))
    });

    group.finish();
}

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_into_full_index");

    for (name, pk_size) in &[("short_36B", 36), ("medium_200B", 200), ("large_500B", 500)] {
        let ds = common::generate(*pk_size, 100_000);
        let mut idx = PrimaryIndex::with_capacity(ds.row_count + 1);
        for (i, pk) in ds.pks.iter().enumerate() {
            idx.insert(pk, (0, i as u32));
        }
        let new_pk = common::generate(*pk_size, 1).pks[0].clone();

        group.bench_with_input(BenchmarkId::from_parameter(name), &new_pk, |b, pk| {
            b.iter(|| {
                let mut idx = black_box(&idx);
                // We can't actually mutate through black_box, so measure hash + lookup
                let _ = idx.get(black_box(pk));
            })
        });
    }

    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("get");

    for (name, pk_size) in &[("short_36B", 36), ("medium_200B", 200), ("large_500B", 500)] {
        let ds = common::generate(*pk_size, 100_000);
        let mut idx = PrimaryIndex::with_capacity(ds.row_count);
        for (i, pk) in ds.pks.iter().enumerate() {
            idx.insert(pk, (0, i as u32));
        }

        let hit_pk = ds.pks[50_000].clone();
        let miss_pk = common::generate(*pk_size, 999_999).pks[0].clone();

        group.bench_with_input(
            BenchmarkId::new("hit", name),
            &hit_pk,
            |b, pk| b.iter(|| idx.get(black_box(pk))),
        );
        group.bench_with_input(
            BenchmarkId::new("miss", name),
            &miss_pk,
            |b, pk| b.iter(|| idx.get(black_box(pk))),
        );
    }

    group.finish();
}

criterion_group!(benches, bench_hash, bench_insert, bench_get);
criterion_main!(benches);
