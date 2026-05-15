//! Exact-index file size and estimated RAM comparison (v3 mmap vs v2 in-memory).
//!
//! Run:  `cargo run --release --bench exact_index_ram`
//! Large: `BITTICE_BENCH_LARGE=1 cargo run --release --bench exact_index_ram`

mod common;

use std::path::Path;

use roaring::RoaringBitmap;
use tempfile::TempDir;

use bittice::core::storage::exact_index::ExactIndex;
use bittice::core::storage::exact_index_v3::write_exact_index_v3;

fn bm(rows: &[u32]) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for &r in rows {
        b.insert(r);
    }
    b
}

fn write_v3(path: &Path, n: usize) {
    let mut entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = (0..n)
        .map(|i| {
            let v = format!("status_{:08}", i);
            (
                ExactIndex::hash(&v),
                vec![((i as u64 % 5) + 1, bm(&[(i % 1000) as u32]))],
            )
        })
        .collect();
    entries.sort_unstable_by_key(|(h, _)| *h);
    write_exact_index_v3(path, entries).expect("write v3");
}

fn main() {
    println!("=== Exact index: on-disk v3 vs estimated v2 RAM ===\n");
    println!(
        "{:>8} | {:>12} | {:>12} | {:>12} | {:>8}",
        "entries", "v3 file", "v2 est RAM", "v1 est RAM", "v2/v3"
    );
    println!("{}", "-".repeat(62));

    for (name, n) in common::exact_index_sizes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exact_Status.idx");
        write_v3(&path, n);

        let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // v2: u128 key (16) + avg ~40B bitmap payload + hashbrown ~1.5×
        let v2_est = (n as u64) * 56 * 3 / 2;
        // v1: String key ~20B avg + same payload + hashbrown
        let v1_est = (n as u64) * 76 * 3 / 2;
        let ratio = if file_bytes > 0 {
            v2_est as f64 / file_bytes as f64
        } else {
            0.0
        };

        println!(
            "{:>8} | {:>9.2} MB | {:>9.2} MB | {:>9.2} MB | {:>6.1}x",
            name,
            file_bytes as f64 / 1_048_576.0,
            v2_est as f64 / 1_048_576.0,
            v1_est as f64 / 1_048_576.0,
            ratio,
        );
    }

    println!();
    println!("v3 query path loads one entry at a time (mmap + binary search).");
    println!("v2 load deserializes the entire HashMap into RAM (see exact_index_ops::exact_v2_full_deserialize).");
    println!("Set BITTICE_BENCH_LARGE=1 to include 1M entries.");
}
