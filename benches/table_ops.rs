mod common;

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::collections::HashMap;
use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

fn setup_table(dir: &std::path::Path, name: &str, ds: &common::BenchDataset) -> Table {
    let mut table = Table::open(dir, name).expect("open");
    table.manifest.primary_key = "PK".to_string();
    table.manifest.original_fields = vec![
        "PK".to_string(), "Name".to_string(), "Value".to_string(),
        "Category".to_string(), "Status".to_string(), "Description".to_string(),
    ];
    for row in &ds.rows {
        table.insert(row.clone()).expect("insert");
    }
    table.flush_active_segment().expect("flush");
    table.close().expect("close");
    Table::open(dir, name).expect("reopen")
}

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("table_insert");

    for (name, pk_size, row_count) in &[
        ("tiny_36B_10K", 36, 10_000usize),
        ("medium_200B_100K", 200, 100_000),
    ] {
        let ds = common::generate(*pk_size, *row_count);
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bench");
        let mut table = setup_table(&dir, "t", &ds);

        let new_pk = common::generate(*pk_size, 1).pks[0].clone();
        let mut new_row = HashMap::new();
        new_row.insert("PK".to_string(), new_pk);
        new_row.insert("Name".to_string(), "bench_name".to_string());
        new_row.insert("Value".to_string(), "bench_val".to_string());
        new_row.insert("Category".to_string(), "bench_cat".to_string());
        new_row.insert("Status".to_string(), "bench_status".to_string());
        new_row.insert("Description".to_string(), "bench_desc".to_string());

        group.bench_with_input(BenchmarkId::from_parameter(name), &new_row, |b, row| {
            b.iter(|| {
                table.insert(black_box(row.clone())).expect("insert");
            })
        });
    }

    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("table_search_pk");

    for (name, pk_size, row_count) in &[
        ("tiny_36B_10K", 36, 10_000usize),
        ("medium_200B_100K", 200, 100_000),
    ] {
        let ds = common::generate(*pk_size, *row_count);
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bench");
        let table = setup_table(&dir, "t", &ds);

        let hit_pk = ds.pks[row_count / 2].clone();
        let miss_pk = common::generate(*pk_size, 999_999).pks[0].clone();

        let fields = vec![
            "PK".to_string(), "Name".to_string(), "Value".to_string(),
        ];

        group.bench_with_input(
            BenchmarkId::new("hit", name),
            &hit_pk,
            |b, pk| {
                let filters = vec![Filter {
                    field: "PK".to_string(),
                    op: ComparisonOp::Eq,
                    value: pk.clone(),
                    value_to: None,
                    field_type: None,
                    value_options: vec![],
                }];
                b.iter(|| {
                    let _ = table.search(
                        black_box(&fields),
                        black_box(&filters),
                        &LogicalOp::And, &[], &[], 100, 0, None,
                    ).expect("search");
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("miss", name),
            &miss_pk,
            |b, pk| {
                let filters = vec![Filter {
                    field: "PK".to_string(),
                    op: ComparisonOp::Eq,
                    value: pk.clone(),
                    value_to: None,
                    field_type: None,
                    value_options: vec![],
                }];
                b.iter(|| {
                    let _ = table.search(
                        black_box(&fields),
                        black_box(&filters),
                        &LogicalOp::And, &[], &[], 100, 0, None,
                    ).expect("search");
                })
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_insert, bench_search);
criterion_main!(benches);
