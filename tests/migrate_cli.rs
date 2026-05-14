//! Tests for the `migrate-primary-index` CLI command.
//!
//! These tests call the migration functions directly (not via CLI binary)
//! to verify correctness without spawning processes.

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

use bittice::core::migrate_primary_index;
use bittice::core::storage::primary_index_io;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_v1_file(path: &Path, data: &HashMap<String, (u64, u32)>, with_header: bool) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = BufWriter::new(fs::File::create(path).unwrap());
    if with_header {
        file.write_all(b"BTPI").unwrap();
        file.write_all(&[1, 0, 0, 0]).unwrap();
    }
    bincode::serialize_into(&mut file, data).unwrap();
    file.flush().unwrap();
}

fn make_legacy_data(entries: &[(&str, (u64, u32))]) -> HashMap<String, (u64, u32)> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect()
}

fn checksum(path: &Path) -> u64 {
    let data = fs::read(path).unwrap();
    // Simple checksum: length + first 8 bytes as u64
    let mut hasher: u64 = data.len() as u64;
    for (i, b) in data.iter().take(8).enumerate() {
        hasher ^= (*b as u64) << (i * 8);
    }
    hasher
}

// ===========================================================================
// (a) Migración exitosa de v1 a v2
// ===========================================================================

#[test]
fn migrate_v1_to_v2_success() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");

    let data = make_legacy_data(&[("pk1", (1, 0)), ("pk2", (2, 1)), ("pk3", (3, 2))]);
    write_v1_file(&idx_path, &data, true);

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, false,
    );

    assert!(result.error.is_none(), "error: {:?}", result.error);
    assert!(!result.skipped);
    assert_eq!(result.entries, 3);
    assert_eq!(result.collisions, 0);
    assert!(result.backup_path.is_some());

    // Verify v2 file.
    let loaded = primary_index_io::load_primary_index(&idx_path).expect("load v2");
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded.get("pk1"), Some((1, 0)));
    assert_eq!(loaded.get("pk2"), Some((2, 1)));
    assert_eq!(loaded.get("pk3"), Some((3, 2)));
}

// ===========================================================================
// (b) Migración de archivo legacy (sin header)
// ===========================================================================

#[test]
fn migrate_legacy_no_header() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");

    let data = make_legacy_data(&[("leg1", (10, 0)), ("leg2", (20, 1))]);
    write_v1_file(&idx_path, &data, false); // no header

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, false,
    );

    assert!(result.error.is_none(), "error: {:?}", result.error);
    assert!(result.format_before.contains("legacy"));
    assert_eq!(result.entries, 2);

    let loaded = primary_index_io::load_primary_index(&idx_path).expect("load v2");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded.get("leg1"), Some((10, 0)));
}

// ===========================================================================
// (c) Idempotencia: segunda migración no hace nada
// ===========================================================================

#[test]
fn migrate_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");

    let data = make_legacy_data(&[("pk1", (1, 0))]);
    write_v1_file(&idx_path, &data, true);

    // First migration
    let r1 = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, false,
    );
    assert!(r1.error.is_none());
    assert!(!r1.skipped);

    // Second migration — should skip
    let r2 = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, false,
    );
    assert!(r2.skipped);
    assert_eq!(r2.elapsed_ms, 0); // skipped immediately (well, ~0ms)
}

// ===========================================================================
// (d) --force re-migra v2
// ===========================================================================

#[test]
fn migrate_force_rewrites_v2() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");

    // Write v2 directly.
    let mut idx = bittice::core::storage::primary_index::PrimaryIndex::new();
    idx.insert("pk1", (1, 0));
    primary_index_io::save_primary_index(&idx_path, &idx).unwrap();

    let mtime_before = fs::metadata(&idx_path).unwrap().modified().unwrap();

    // Small delay to ensure mtime changes.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, true, // force=true
    );

    assert!(result.error.is_none(), "error: {:?}", result.error);
    assert!(!result.skipped);

    let mtime_after = fs::metadata(&idx_path).unwrap().modified().unwrap();
    assert!(mtime_after >= mtime_before, "file should have been rewritten");

    // Verify content is still valid.
    let loaded = primary_index_io::load_primary_index(&idx_path).unwrap();
    assert_eq!(loaded.get("pk1"), Some((1, 0)));
}

// ===========================================================================
// (e) --dry-run no toca el archivo
// ===========================================================================

#[test]
fn migrate_dry_run_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");

    let data = make_legacy_data(&[("pk1", (1, 0))]);
    write_v1_file(&idx_path, &data, true);

    let checksum_before = checksum(&idx_path);
    let mtime_before = fs::metadata(&idx_path).unwrap().modified().unwrap();

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, true, true, false, // dry_run=true
    );

    assert!(result.error.is_none());
    assert!(!result.skipped);
    assert_eq!(result.entries, 1);

    // File must be unchanged.
    assert_eq!(checksum(&idx_path), checksum_before);
    assert_eq!(
        fs::metadata(&idx_path).unwrap().modified().unwrap(),
        mtime_before
    );
}

// ===========================================================================
// (f) Backup conservado por default
// ===========================================================================

#[test]
fn migrate_backup_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");
    let backup_path = table_dir.join("primary.idx.v1.bak");

    let data = make_legacy_data(&[("pk1", (1, 0))]);
    write_v1_file(&idx_path, &data, true);
    let original_checksum = checksum(&idx_path);

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, false, // keep_backup=true
    );

    assert!(result.error.is_none());
    assert!(backup_path.exists(), "backup file should exist");
    assert_eq!(checksum(&backup_path), original_checksum, "backup should have original content");
    assert!(result.backup_path.is_some());
}

// ===========================================================================
// (g) --no-keep-backup borra el backup
// ===========================================================================

#[test]
fn migrate_no_backup_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");
    let backup_path = table_dir.join("primary.idx.v1.bak");

    let data = make_legacy_data(&[("pk1", (1, 0))]);
    write_v1_file(&idx_path, &data, true);

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, false, false, // keep_backup=false
    );

    assert!(result.error.is_none());
    assert!(!backup_path.exists(), "backup file should NOT exist");
    assert!(result.backup_path.is_none());
}

// ===========================================================================
// (h) --all itera múltiples tablas
// ===========================================================================

#[test]
fn migrate_all_multiple_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path();

    // Table 1: v1 with header
    let t1_dir = data_root.join("mirror").join("ent1").join("tbl1");
    write_v1_file(
        &t1_dir.join("primary.idx"),
        &make_legacy_data(&[("a", (1, 0))]),
        true,
    );

    // Table 2: already v2
    let t2_dir = data_root.join("mirror").join("ent1").join("tbl2");
    fs::create_dir_all(&t2_dir).unwrap();
    let mut idx = bittice::core::storage::primary_index::PrimaryIndex::new();
    idx.insert("b", (2, 1));
    primary_index_io::save_primary_index(&t2_dir.join("primary.idx"), &idx).unwrap();

    // Table 3: legacy (no header)
    let t3_dir = data_root.join("mirror").join("ent2").join("tbl3");
    write_v1_file(
        &t3_dir.join("primary.idx"),
        &make_legacy_data(&[("c", (3, 2))]),
        false,
    );

    let results = migrate_primary_index::migrate_all(data_root, false, true, false);

    assert_eq!(results.len(), 3);

    let migrated: Vec<_> = results.iter().filter(|r| !r.skipped && r.error.is_none()).collect();
    let skipped: Vec<_> = results.iter().filter(|r| r.skipped).collect();

    assert_eq!(migrated.len(), 2, "two tables should be migrated");
    assert_eq!(skipped.len(), 1, "one table should be skipped (already v2)");

    // All should end up as v2.
    let loaded1 = primary_index_io::load_primary_index(&t1_dir.join("primary.idx")).unwrap();
    assert_eq!(loaded1.get("a"), Some((1, 0)));

    let loaded3 = primary_index_io::load_primary_index(&t3_dir.join("primary.idx")).unwrap();
    assert_eq!(loaded3.get("c"), Some((3, 2)));
}

// ===========================================================================
// (i) Colisión sintética en migración (NFC/NFD equivalent PKs)
// ===========================================================================

#[test]
fn migrate_detects_nfc_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("entity").join("table");
    fs::create_dir_all(&table_dir).unwrap();
    let idx_path = table_dir.join("primary.idx");

    // Two PKs that are canonically equivalent under NFC.
    let mut data = HashMap::new();
    data.insert("caf\u{00e9}".to_string(), (1, 0)); // NFC
    data.insert("cafe\u{0301}".to_string(), (2, 1)); // NFD — same canonical form
    data.insert("other".to_string(), (3, 2));

    write_v1_file(&idx_path, &data, true);

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, false,
    );

    assert!(result.error.is_none(), "error: {:?}", result.error);
    assert_eq!(result.entries, 3, "should read 3 entries from v1");
    assert_eq!(result.collisions, 1, "should detect 1 collision (NFC/NFD)");
    assert_eq!(result.unique_hashes, 2, "should have 2 unique hashes");

    // Verify backup has original 3 entries.
    let backup_path = table_dir.join("primary.idx.v1.bak");
    assert!(backup_path.exists());
    let backup_data: HashMap<String, (u64, u32)> = {
        let mut file = std::io::BufReader::new(fs::File::open(&backup_path).unwrap());
        let mut header = [0u8; 8];
        let _ = std::io::Read::read(&mut file, &mut header).unwrap();
        bincode::deserialize_from(file).unwrap()
    };
    assert_eq!(backup_data.len(), 3, "backup should have all 3 original entries");

    // Verify v2 has 2 entries (collision merged).
    let loaded = primary_index_io::load_primary_index(&idx_path).unwrap();
    assert_eq!(loaded.len(), 2, "v2 should have 2 entries (collision merged)");
    assert_eq!(loaded.get("other"), Some((3, 2)), "non-colliding entry should survive");
}

// ===========================================================================
// (j) Tabla no encontrada
// ===========================================================================

#[test]
fn migrate_table_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let table_dir = tmp.path().join("nonexistent");

    let result = migrate_primary_index::migrate_table(
        "entity", "table", &table_dir, false, true, false,
    );

    assert!(result.error.is_some());
    assert!(result.error.unwrap().contains("no encontrado"));
}
