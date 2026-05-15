use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use roaring::RoaringBitmap;
use tracing::{info, warn};

use super::exact_index::ExactIndex;

const EXACT_IDX_MAGIC: [u8; 4] = *b"BTXI";
const EXACT_IDX_VERSION_V1: u8 = 1;
const EXACT_IDX_VERSION_V2: u8 = 2;

/// Save an `ExactIndex` to disk atomically (v2 format).
///
/// Format (v2):
/// ```text
/// [magic: 4 bytes = "BTXI"][version: u8 = 2][reserved: 3 bytes = 0,0,0]
/// [payload: bincode-serialized ExactIndex (transparent HashMap<u128, ...>)]
/// ```
///
/// The write is atomic: a `.tmp` file is written first, then renamed.
pub fn save_exact_index(path: &Path, idx: &ExactIndex) -> Result<()> {
    let dir = path.parent().context("exact index path has no parent")?;
    if !dir.exists() {
        fs::create_dir_all(dir)
            .with_context(|| format!("create dir {:?}", dir))?;
    }
    let tmp_path = path.with_extension("idx.tmp");
    let mut file = BufWriter::new(
        fs::File::create(&tmp_path)
            .with_context(|| format!("create tmp file {:?}", tmp_path))?,
    );
    file.write_all(&EXACT_IDX_MAGIC)?;
    file.write_all(&[EXACT_IDX_VERSION_V2, 0, 0, 0])?;
    bincode::serialize_into(&mut file, idx)
        .context("serialize ExactIndex v2 payload")?;
    file.flush()?;
    file.into_inner()
        .context("BufWriter into_inner failed")?
        .sync_all()?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp_path, path))?;
    Ok(())
}

/// Load an `ExactIndex` from disk, auto-detecting the format.
///
/// - **v2** (magic "BTXI" + version 2): deserializes directly as `ExactIndex`
///   with `HashMap<u128, _>` keys.
/// - **v1** (magic "BTXI" + version 1): loads legacy `HashMap<String, _>`,
///   rehashes all keys via `xxh3_128(canonical_bytes(value))`, returns migrated
///   `ExactIndex`.
/// - **Legacy** (no "BTXI" magic): same as v1 migration but treats the whole
///   file as raw bincode `HashMap<String, _>`.
/// - **Unknown version**: returns an error.
pub fn load_exact_index(path: &Path) -> Result<ExactIndex> {
    let mut file = BufReader::new(
        fs::File::open(path).with_context(|| format!("open {:?}", path))?,
    );
    let mut header = [0u8; 8];
    let n = file.read(&mut header)?;

    if n >= 4 && header[..4] == EXACT_IDX_MAGIC {
        let version = header[4];
        match version {
            EXACT_IDX_VERSION_V2 => {
                let idx: ExactIndex = bincode::deserialize_from(file)
                    .context("failed to deserialize exact index v2 payload")?;
                Ok(idx)
            }
            EXACT_IDX_VERSION_V1 => {
                info!(
                    "exact index at {:?} is v1 (String keys); migrating to v2 in memory",
                    path
                );
                let raw: HashMap<String, Vec<(u64, RoaringBitmap)>> =
                    bincode::deserialize_from(file)
                        .context("failed to deserialize exact index v1 payload")?;
                Ok(migrate_v1_to_v2(raw, path))
            }
            v => Err(anyhow::anyhow!(
                "exact index version {} not supported by this binary (path: {:?})",
                v,
                path
            )),
        }
    } else {
        // Legacy file (no header) — deserialize whole file as raw HashMap<String, _>.
        warn!(
            "exact index at {:?} is in legacy format (no header); \
             migrating to v2 in memory, will persist as v2 on next save",
            path
        );
        let file = BufReader::new(
            fs::File::open(path).with_context(|| format!("reopen legacy {:?}", path))?,
        );
        let raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = bincode::deserialize_from(file)
            .context("failed to deserialize legacy exact index")?;
        Ok(migrate_v1_to_v2(raw, path))
    }
}

/// Migrate a `HashMap<String, _>` (v1 or legacy) into an `ExactIndex` (v2).
///
/// Rehashes each key via `xxh3_128(canonical_bytes(value))`.  If two distinct
/// String keys map to the same hash (NFC collapse or true collision), their
/// bitmap lists are merged with `insert_raw_or_merge` and a warning is emitted.
fn migrate_v1_to_v2(
    raw: HashMap<String, Vec<(u64, RoaringBitmap)>>,
    path: &Path,
) -> ExactIndex {
    let mut idx = ExactIndex::with_capacity(raw.len());
    let mut collapsed = 0usize;
    for (value, bitmaps) in raw {
        let hash = ExactIndex::hash(&value);
        if idx.insert_raw_or_merge(hash, bitmaps) {
            collapsed += 1;
        }
    }
    if collapsed > 0 {
        warn!(
            "exact index migration {:?}: {} entries collapsed (NFC normalization merged distinct \
             String keys to the same hash). This is expected when migrating NFD/NFC variants.",
            path, collapsed
        );
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use roaring::RoaringBitmap;
    use std::collections::HashMap;
    use std::io::BufWriter;

    fn make_test_index() -> ExactIndex {
        let mut idx = ExactIndex::new();
        let mut bm1 = RoaringBitmap::new();
        bm1.insert(0);
        bm1.insert(5);
        idx.insert("foo", vec![(1u64, bm1)]);
        let mut bm2 = RoaringBitmap::new();
        bm2.insert(42);
        idx.insert("bar", vec![(2u64, bm2)]);
        idx
    }

    // -----------------------------------------------------------------------
    // Test 1: save with v2 function, load, verify equality
    // -----------------------------------------------------------------------

    #[test]
    fn io_save_load_roundtrip_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("exact_test.idx");

        let original = make_test_index();
        save_exact_index(&path, &original).expect("save");

        // Verify magic bytes and version in file.
        let raw = fs::read(&path).unwrap();
        assert_eq!(&raw[..4], b"BTXI", "magic must be BTXI");
        assert_eq!(raw[4], 2, "version must be 2 (v2)");

        let loaded = load_exact_index(&path).expect("load");
        assert_eq!(loaded.len(), original.len());

        let foo = loaded.get("foo").expect("foo should exist");
        assert_eq!(foo.len(), 1);
        assert_eq!(foo[0].0, 1u64);
        assert!(foo[0].1.contains(0));
        assert!(foo[0].1.contains(5));

        let bar = loaded.get("bar").expect("bar should exist");
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].0, 2u64);
        assert!(bar[0].1.contains(42));
    }

    // -----------------------------------------------------------------------
    // Test 2: legacy file (no magic) — load migrates to v2 in memory
    // -----------------------------------------------------------------------

    #[test]
    fn io_load_legacy_no_header() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("exact_legacy.idx");

        // Write a raw HashMap<String, _> bincode (legacy format, no BTXI header).
        let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
        let mut bm = RoaringBitmap::new();
        bm.insert(7);
        raw.insert("legacy_val".to_string(), vec![(99u64, bm.clone())]);

        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            bincode::serialize_into(&mut file, &raw).unwrap();
            file.flush().unwrap();
        }

        // Should not panic, and data should be correct.
        let idx = load_exact_index(&path).expect("load legacy");
        assert_eq!(idx.len(), 1);
        let entries = idx.get("legacy_val").expect("legacy_val should exist");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 99u64);
        assert_eq!(entries[0].1, bm);
    }

    // -----------------------------------------------------------------------
    // Test 3: BTXI magic + unknown version → error
    // -----------------------------------------------------------------------

    #[test]
    fn io_load_unknown_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("exact_unknown.idx");

        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            file.write_all(&EXACT_IDX_MAGIC).unwrap();
            file.write_all(&[99u8, 0, 0, 0]).unwrap();
            file.write_all(&[0u8; 16]).unwrap();
            file.flush().unwrap();
        }

        let result = load_exact_index(&path);
        assert!(result.is_err(), "unknown version should return error");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("99"),
            "error must mention version 99, got: {}",
            msg
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: load legacy → save → load again, second load sees v2 header
    // -----------------------------------------------------------------------

    #[test]
    fn io_silent_migration_to_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("exact_migrate.idx");

        // Write legacy file (no header).
        let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
        let mut bm = RoaringBitmap::new();
        bm.insert(3);
        raw.insert("migrated".to_string(), vec![(5u64, bm.clone())]);
        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            bincode::serialize_into(&mut file, &raw).unwrap();
            file.flush().unwrap();
        }

        // First load: legacy format, migrated to ExactIndex in memory.
        let loaded = load_exact_index(&path).expect("load legacy");
        assert_eq!(loaded.get("migrated").unwrap()[0].0, 5u64);

        // Save back: this time with v2 header.
        save_exact_index(&path, &loaded).expect("save");

        // Verify file now has the BTXI v2 header.
        let raw_bytes = fs::read(&path).unwrap();
        assert_eq!(&raw_bytes[..4], b"BTXI", "after save should have BTXI magic");
        assert_eq!(raw_bytes[4], 2, "version should be 2");

        // Second load: reads v2 header, returns correct data.
        let reloaded = load_exact_index(&path).expect("load v2");
        assert_eq!(reloaded.len(), 1);
        let entries = reloaded.get("migrated").expect("migrated should exist");
        assert_eq!(entries[0].0, 5u64);
        assert_eq!(entries[0].1, bm);
    }

    // -----------------------------------------------------------------------
    // Test 5: v1 header (BTXI + version 1) — load migrates to v2 in memory
    // -----------------------------------------------------------------------

    #[test]
    fn io_load_v1_header() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("exact_v1.idx");

        // Write a v1 file manually: BTXI + version 1 + bincode HashMap<String, _>.
        let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
        let mut bm = RoaringBitmap::new();
        bm.insert(11);
        raw.insert("v1_value".to_string(), vec![(7u64, bm.clone())]);

        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            file.write_all(&EXACT_IDX_MAGIC).unwrap();
            file.write_all(&[EXACT_IDX_VERSION_V1, 0, 0, 0]).unwrap();
            bincode::serialize_into(&mut file, &raw).unwrap();
            file.flush().unwrap();
        }

        let idx = load_exact_index(&path).expect("load v1");
        assert_eq!(idx.len(), 1);
        let entries = idx.get("v1_value").expect("v1_value should exist");
        assert_eq!(entries[0].0, 7u64);
        assert_eq!(entries[0].1, bm);
    }
}
