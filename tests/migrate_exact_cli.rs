//! Tests for the `migrate-exact-index` CLI migration functions.

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

use bittice::core::migrate_exact_index;
use bittice::core::storage::exact_index::ExactIndex;
use bittice::core::storage::exact_index_v3::reader::SnapshotReader;
use roaring::RoaringBitmap;

fn write_v2_exact(path: &Path, data: &HashMap<String, Vec<(u64, RoaringBitmap)>>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut map: HashMap<u128, Vec<(u64, RoaringBitmap)>> = HashMap::new();
    for (value, bitmaps) in data {
        map.insert(ExactIndex::hash(value), bitmaps.clone());
    }
    let mut file = BufWriter::new(fs::File::create(path).unwrap());
    file.write_all(b"BTXI").unwrap();
    file.write_all(&[2u8, 0, 0, 0]).unwrap();
    bincode::serialize_into(&mut file, &map).unwrap();
    file.flush().unwrap();
}

fn write_v1_exact(path: &Path, data: &HashMap<String, Vec<(u64, RoaringBitmap)>>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = BufWriter::new(fs::File::create(path).unwrap());
    file.write_all(b"BTXI").unwrap();
    file.write_all(&[1u8, 0, 0, 0]).unwrap();
    bincode::serialize_into(&mut file, &data).unwrap();
    file.flush().unwrap();
}

fn bm(values: &[u32]) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for &v in values {
        b.insert(v);
    }
    b
}

fn checksum(path: &Path) -> u64 {
    let data = fs::read(path).unwrap();
    let mut hasher: u64 = data.len() as u64;
    for (i, b) in data.iter().take(8).enumerate() {
        hasher ^= (*b as u64) << (i * 8);
    }
    hasher
}

fn exact_dir(table_dir: &Path) -> std::path::PathBuf {
    table_dir.join("secondary_exact")
}

#[test]
fn migrate_exact_v2_to_v3_success() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    let idx_path = exact_dir(&table_dir).join("exact_Status.idx");

    let mut data = HashMap::new();
    data.insert("active".to_string(), vec![(1u64, bm(&[1, 2]))]);
    data.insert("inactive".to_string(), vec![(1u64, bm(&[3]))]);
    write_v2_exact(&idx_path, &data);

    let results = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, None, false, true, false,
    );
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(r.error.is_none(), "{:?}", r.error);
    assert!(!r.skipped);
    assert_eq!(r.entries, 2);
    assert!(r.backup_path.is_some());

    let raw = fs::read(&idx_path).unwrap();
    assert_eq!(raw[4], 3);

    let reader = SnapshotReader::open(&idx_path).unwrap();
    assert_eq!(reader.num_entries(), 2);
    let active = reader
        .get(ExactIndex::hash("active"))
        .unwrap()
        .expect("active");
    assert!(active[0].1.contains(1));
}

#[test]
fn migrate_exact_v1_to_v3_success() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    let idx_path = exact_dir(&table_dir).join("exact_Email.idx");

    let mut data = HashMap::new();
    data.insert("user@example.com".to_string(), vec![(1u64, bm(&[10]))]);
    write_v1_exact(&idx_path, &data);

    let results = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, None, false, true, false,
    );
    assert!(results[0].error.is_none());
    assert_eq!(fs::read(&idx_path).unwrap()[4], 3);
}

#[test]
fn migrate_exact_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    let idx_path = exact_dir(&table_dir).join("exact_Field.idx");

    let mut data = HashMap::new();
    data.insert("x".to_string(), vec![(1u64, bm(&[1]))]);
    write_v2_exact(&idx_path, &data);

    let r1 = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, None, false, true, false,
    );
    assert!(!r1[0].skipped);

    let r2 = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, None, false, true, false,
    );
    assert!(r2[0].skipped);
}

#[test]
fn migrate_exact_dry_run_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    let idx_path = exact_dir(&table_dir).join("exact_Field.idx");

    let mut data = HashMap::new();
    data.insert("x".to_string(), vec![(1u64, bm(&[1]))]);
    write_v2_exact(&idx_path, &data);

    let cs_before = checksum(&idx_path);
    let results = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, None, true, true, false,
    );
    assert!(results[0].error.is_none());
    assert_eq!(checksum(&idx_path), cs_before);
    assert_eq!(fs::read(&idx_path).unwrap()[4], 2);
}

#[test]
fn migrate_exact_backup_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    let idx_path = exact_dir(&table_dir).join("exact_Field.idx");
    let backup_path = idx_path.with_extension("idx.pre_v3.bak");

    let mut data = HashMap::new();
    data.insert("x".to_string(), vec![(1u64, bm(&[1]))]);
    write_v2_exact(&idx_path, &data);
    let original = checksum(&idx_path);

    let results = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, None, false, true, false,
    );
    assert!(results[0].error.is_none());
    assert!(backup_path.exists());
    assert_eq!(checksum(&backup_path), original);
}

#[test]
fn migrate_exact_single_field_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    let status_path = exact_dir(&table_dir).join("exact_Status.idx");
    let email_path = exact_dir(&table_dir).join("exact_Email.idx");

    let mut d1 = HashMap::new();
    d1.insert("a".to_string(), vec![(1u64, bm(&[1]))]);
    write_v2_exact(&status_path, &d1);

    let mut d2 = HashMap::new();
    d2.insert("b@x.com".to_string(), vec![(1u64, bm(&[2]))]);
    write_v2_exact(&email_path, &d2);

    let results = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, Some("Email"), false, true, false,
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].field, "Email");
    assert_eq!(fs::read(&email_path).unwrap()[4], 3);
    assert_eq!(fs::read(&status_path).unwrap()[4], 2, "Status should stay v2");
}

#[test]
fn migrate_exact_all_multiple_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path();

    let t1 = data_root.join("mirror").join("ent1").join("tbl1");
    let t2 = data_root.join("mirror").join("ent1").join("tbl2");

    let mut d1 = HashMap::new();
    d1.insert("v1".to_string(), vec![(1u64, bm(&[1]))]);
    write_v2_exact(&exact_dir(&t1).join("exact_A.idx"), &d1);

    // Already v3
    let mut idx = ExactIndex::new();
    idx.insert("v2", vec![(2u64, bm(&[2]))]);
    idx.save(Some(&exact_dir(&t2).join("exact_B.idx"))).unwrap();

    let results = migrate_exact_index::migrate_all(data_root, false, true, false);
    assert_eq!(results.len(), 2);

    let migrated = results.iter().filter(|r| !r.skipped && r.error.is_none()).count();
    let skipped = results.iter().filter(|r| r.skipped).count();
    assert_eq!(migrated, 1);
    assert_eq!(skipped, 1);

    assert_eq!(fs::read(exact_dir(&t1).join("exact_A.idx")).unwrap()[4], 3);
}

#[test]
fn migrate_exact_table_no_secondary_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();

    let results = migrate_exact_index::migrate_table(
        "entity", "table", &table_dir, None, false, true, false,
    );
    assert_eq!(results.len(), 1);
    assert!(results[0].error.is_some());
}
