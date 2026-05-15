//! Criterion benchmarks for exact-index v3 lazy lookup vs in-memory v2 load.
//!
//! Run:  `cargo bench --bench exact_index_ops`
//! Large: `BITTICE_BENCH_LARGE=1 cargo bench --bench exact_index_ops`

mod common;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use roaring::RoaringBitmap;
use tempfile::TempDir;

use bittice::core::storage::canonical::canonical_bytes;
use bittice::core::storage::exact_index::ExactIndex;
use bittice::core::storage::exact_index_v3::{write_exact_index_v3, SnapshotReader};
use xxhash_rust::xxh3::xxh3_128;

struct V3Fixture {
    _dir: TempDir,
    path: PathBuf,
    values: Vec<String>,
    hit_value: String,
    miss_value: String,
}

fn bm(rows: &[u32]) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for &r in rows {
        b.insert(r);
    }
    b
}

fn write_v3_snapshot(path: &Path, values: &[String]) {
    let mut entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                ExactIndex::hash(v),
                vec![((i as u64 % 5) + 1, bm(&[(i % 1000) as u32]))],
            )
        })
        .collect();
    entries.sort_unstable_by_key(|(h, _)| *h);
    write_exact_index_v3(path, entries).expect("write v3 snapshot");
}

fn v3_fixture(n: usize) -> V3Fixture {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("exact_Status.idx");
    let values = common::exact_values(n);
    write_v3_snapshot(&path, &values);
    let hit_value = values[n / 2].clone();
    let miss_value = format!("missing_{:08}", n + 99_999);
    V3Fixture {
        _dir: dir,
        path,
        values,
        hit_value,
        miss_value,
    }
}

fn write_v2_payload(path: &Path, values: &[String]) {
    let map: HashMap<u128, Vec<(u64, RoaringBitmap)>> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                ExactIndex::hash(v),
                vec![((i as u64 % 5) + 1, bm(&[(i % 1000) as u32]))],
            )
        })
        .collect();
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    file.write_all(b"BTXI").unwrap();
    file.write_all(&[2u8, 0, 0, 0]).unwrap();
    bincode::serialize_into(&mut file, &map).unwrap();
    file.flush().unwrap();
}

fn bench_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_hash");
    let short = "active".to_string();
    let medium = format!("user@example.com/{}", "x".repeat(80));
    let long = format!("desc_{}", "y".repeat(200));

    group.bench_function("short", |b| {
        b.iter(|| ExactIndex::hash(black_box(&short)))
    });
    group.bench_function("medium_100B", |b| {
        b.iter(|| ExactIndex::hash(black_box(&medium)))
    });
    group.bench_function("long_250B", |b| {
        b.iter(|| ExactIndex::hash(black_box(&long)))
    });
    group.bench_function("canonical_xxh3_only", |b| {
        b.iter(|| xxh3_128(&canonical_bytes(black_box(&medium))))
    });
    group.finish();
}

fn bench_open(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_open_v3");
    group.sample_size(20);

    for (name, n) in common::exact_index_sizes() {
        let fx = v3_fixture(n);
        group.bench_with_input(BenchmarkId::from_parameter(name), &fx.path, |b, path| {
            b.iter(|| ExactIndex::open(black_box(path)).expect("open"));
        });
    }
    group.finish();
}

fn bench_open_get_once(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_open_get_once");
    group.sample_size(20);

    for (name, n) in common::exact_index_sizes() {
        let fx = v3_fixture(n);
        let hit = fx.hit_value.clone();
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(fx.path, hit),
            |b, (path, value)| {
                b.iter(|| {
                    let idx = ExactIndex::open(black_box(path)).expect("open");
                    let got = idx.get(black_box(value));
                    black_box(got);
                })
            },
        );
    }
    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_get_v3");

    for (name, n) in common::exact_index_sizes() {
        let fx = v3_fixture(n);
        let idx = ExactIndex::open(&fx.path).expect("open");

        group.bench_with_input(
            BenchmarkId::new("hit_cold", name),
            &fx.hit_value,
            |b, value| {
                b.iter(|| {
                    let got = idx.get(black_box(value));
                    black_box(got);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("miss", name),
            &fx.miss_value,
            |b, value| {
                b.iter(|| {
                    let got = idx.get(black_box(value));
                    black_box(got);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("hit_warm_x10", name),
            &fx.hit_value,
            |b, value| {
                b.iter(|| {
                    for _ in 0..10 {
                        let got = idx.get(black_box(value));
                        black_box(got);
                    }
                })
            },
        );
    }
    group.finish();
}

fn bench_get_distinct(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_get_distinct_v3");
    group.sample_size(20);

    for (name, n) in &[("10k", 10_000usize), ("100k", 100_000usize)] {
        let fx = v3_fixture(*n);
        let idx = ExactIndex::open(&fx.path).expect("open");
        let sample: Vec<String> = (0..10)
            .map(|i| fx.values[i * (n / 10)].clone())
            .collect();

        group.bench_with_input(BenchmarkId::from_parameter(name), &sample, |b, keys| {
            b.iter(|| {
                for key in keys {
                    let got = idx.get(black_box(key));
                    black_box(got);
                }
            })
        });
    }
    group.finish();
}

fn bench_snapshot_reader(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_snapshot_reader_get");

    for (name, n) in common::exact_index_sizes() {
        let fx = v3_fixture(n);
        let reader = SnapshotReader::open(&fx.path).expect("reader");
        let hit_hash = ExactIndex::hash(&fx.hit_value);
        let miss_hash = ExactIndex::hash(&fx.miss_value);

        group.bench_with_input(
            BenchmarkId::new("hit", name),
            &hit_hash,
            |b, hash| {
                b.iter(|| {
                    let got = reader.get(black_box(*hash)).expect("get");
                    black_box(got);
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("miss", name),
            &miss_hash,
            |b, hash| {
                b.iter(|| {
                    let got = reader.get(black_box(*hash)).expect("get");
                    black_box(got);
                })
            },
        );
    }
    group.finish();
}

fn bench_v2_full_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_v2_full_deserialize");
    group.sample_size(10);

    for (name, n) in &[("10k", 10_000usize), ("100k", 100_000usize)] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exact_v2.idx");
        let values = common::exact_values(*n);
        write_v2_payload(&path, &values);

        group.bench_with_input(BenchmarkId::from_parameter(name), &path, |b, path| {
            b.iter(|| {
                let mut file = std::io::BufReader::new(
                    std::fs::File::open(black_box(path)).expect("open"),
                );
                let mut header = [0u8; 8];
                file.read_exact(&mut header).expect("header");
                let map: HashMap<u128, Vec<(u64, RoaringBitmap)>> =
                    bincode::deserialize_from(file).expect("deserialize");
                black_box(map.len())
            })
        });
    }
    group.finish();
}

fn bench_save_noop(c: &mut Criterion) {
    let mut group = c.benchmark_group("exact_save_noop");
    group.sample_size(20);

    for (name, n) in common::exact_index_sizes() {
        let fx = v3_fixture(n);
        group.bench_with_input(BenchmarkId::from_parameter(name), &fx.path, |b, path| {
            b.iter(|| {
                let mut idx = ExactIndex::open(black_box(path)).expect("open");
                idx.save(Some(path)).expect("save noop");
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_hash,
    bench_open,
    bench_open_get_once,
    bench_get,
    bench_get_distinct,
    bench_snapshot_reader,
    bench_v2_full_load,
    bench_save_noop,
);
criterion_main!(benches);
