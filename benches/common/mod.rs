use std::collections::HashMap;

pub struct BenchDataset {
    pub pk_size_bytes: usize,
    pub row_count: usize,
    pub pks: Vec<String>,
    pub rows: Vec<HashMap<String, String>>,
}

/// Generate a deterministic dataset for benchmarks.
pub fn generate(pk_size_bytes: usize, row_count: usize) -> BenchDataset {
    let mut pks = Vec::with_capacity(row_count);
    let mut rows = Vec::with_capacity(row_count);

    for i in 0..row_count {
        let pk = make_pk(i, pk_size_bytes);
        let mut row = HashMap::new();
        row.insert("PK".to_string(), pk.clone());
        row.insert("Name".to_string(), format!("name_{}", i));
        row.insert("Value".to_string(), format!("value_{}", i));
        row.insert("Category".to_string(), format!("cat_{}", i % 100));
        row.insert("Status".to_string(), format!("status_{}", i % 10));
        row.insert("Description".to_string(), format!("desc_{}", i));
        pks.push(pk);
        rows.push(row);
    }

    BenchDataset {
        pk_size_bytes,
        row_count,
        pks,
        rows,
    }
}

fn make_pk(i: usize, target_size: usize) -> String {
    let base = format!("{:08}", i);
    if target_size <= base.len() {
        return base[..target_size].to_string();
    }
    let filler_len = target_size - base.len();
    let filler = "x".repeat(filler_len);
    format!("{}{}", base, filler)
}

/// Generate a dataset with variable-length PKs (10 to 200 bytes).
pub fn generate_variable(row_count: usize) -> BenchDataset {
    let mut pks = Vec::with_capacity(row_count);
    let mut rows = Vec::with_capacity(row_count);

    for i in 0..row_count {
        // Variable length: cycles between 10 and 200 bytes
        let pk_size = 10 + (i * 190 / row_count.max(1));
        let pk = make_pk(i, pk_size);
        let mut row = HashMap::new();
        row.insert("PK".to_string(), pk.clone());
        row.insert("Name".to_string(), format!("name_{}", i));
        row.insert("Value".to_string(), format!("value_{}", i));
        pks.push(pk);
        rows.push(row);
    }

    BenchDataset {
        pk_size_bytes: 0, // variable
        row_count,
        pks,
        rows,
    }
}

/// All PK shapes for the generalization matrix.
pub fn pk_shapes() -> Vec<(&'static str, usize)> {
    vec![
        ("uuid_36b", 36),
        ("numeric_short_10b", 10),
        ("numeric_long_20b", 20),
        ("composite_100b", 100),
        ("wide_pk_500b", 500),
    ]
}

/// All table sizes for the generalization matrix.
pub fn table_sizes() -> Vec<(&'static str, usize)> {
    vec![
        ("10k", 10_000),
        ("100k", 100_000),
    ]
}

/// Table sizes including large (opt-in via BITTICE_BENCH_LARGE).
pub fn table_sizes_with_large() -> Vec<(&'static str, usize)> {
    let mut sizes = table_sizes();
    if std::env::var("BITTICE_BENCH_LARGE").is_ok() {
        sizes.push(("1m", 1_000_000));
    }
    sizes
}

/// Distinct-value counts for exact-index benchmarks.
pub fn exact_index_sizes() -> Vec<(&'static str, usize)> {
    let mut sizes = vec![("10k", 10_000), ("100k", 100_000)];
    if std::env::var("BITTICE_BENCH_LARGE").is_ok() {
        sizes.push(("1m", 1_000_000));
    }
    sizes
}

/// Build `n` distinct field values for exact-index benchmarks.
pub fn exact_values(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("status_{:08}", i)).collect()
}
