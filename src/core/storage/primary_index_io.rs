use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::canonical::canonical_bytes;
use super::primary_index::PrimaryIndex;
use xxhash_rust::xxh3::xxh3_128;

const PRIMARY_IDX_MAGIC: [u8; 4] = *b"BTPI";
const PRIMARY_IDX_VERSION_V1: u8 = 1;
const PRIMARY_IDX_VERSION_V2: u8 = 2;
const PRIMARY_IDX_CURRENT_VERSION: u8 = PRIMARY_IDX_VERSION_V2;

/// Save a `PrimaryIndex` to disk with a v2 version header.
///
/// Format (v2):
/// ```text
/// [magic: 4 bytes = "BTPI"][version: u8 = 2][reserved: 3 bytes = 0]
/// [payload: bincode-serialized HashMap<u128, (u64, u32)>]
/// ```
///
/// The first time the new code closes a table that was loaded from v1 or
/// legacy format, it rewrites `primary.idx` with this v2 header. No
/// explicit migration command is needed for the v1→v2 transition.
pub fn save_primary_index(path: &Path, idx: &PrimaryIndex) -> Result<()> {
    let tmp_path = path.with_extension("idx.tmp");
    let mut file = BufWriter::new(fs::File::create(&tmp_path)?);
    file.write_all(&PRIMARY_IDX_MAGIC)?;
    file.write_all(&[PRIMARY_IDX_CURRENT_VERSION, 0, 0, 0])?;
    bincode::serialize_into(&mut file, idx.inner())?;
    file.flush()?;
    file.into_inner()
        .context("BufWriter into_inner failed")?
        .sync_all()?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Load a `PrimaryIndex` from disk, auto-detecting the format.
///
/// - **v2** (magic + version 2): deserializes `HashMap<u128, _>` directly.
/// - **v1** (magic + version 1): deserializes `HashMap<String, _>`, hashes
///   each key to build a v2 `PrimaryIndex` in memory. Will be persisted
///   as v2 on the next close.
/// - **Legacy** (no magic): treats as v1. Emits a warning.
pub fn load_primary_index(path: &Path) -> Result<PrimaryIndex> {
    let mut file = BufReader::new(fs::File::open(path)?);
    let mut header = [0u8; 8];
    let n = file.read(&mut header)?;

    if n >= 4 && header[..4] == PRIMARY_IDX_MAGIC {
        let version = header[4];
        match version {
            PRIMARY_IDX_VERSION_V2 => {
                let idx: PrimaryIndex = bincode::deserialize_from(file)
                    .context("failed to deserialize primary.idx v2 payload")?;
                Ok(idx)
            }
            PRIMARY_IDX_VERSION_V1 => load_v1_migrate(file, path),
            v => Err(anyhow::anyhow!(
                "primary.idx version {} not supported by this binary (path: {:?})",
                v,
                path
            )),
        }
    } else {
        // Legacy file (no header) — treat as v1.
        let file = BufReader::new(fs::File::open(path)?);
        warn!(
            "primary.idx in legacy format (no header) at {:?}; \
             migrating in memory, will persist as v2 on next close",
            path
        );
        load_v1_migrate(file, path)
    }
}

/// Read a v1 payload (`HashMap<String, (u64, u32)>`) and build a v2
/// `PrimaryIndex` by hashing each PK. The migration to v2 on disk
/// happens on the next `save_primary_index` call (i.e. next close).
fn load_v1_migrate<R: Read>(reader: R, path: &Path) -> Result<PrimaryIndex> {
    let legacy: HashMap<String, (u64, u32)> = bincode::deserialize_from(reader)
        .context("failed to deserialize legacy primary.idx")?;

    let mut idx = PrimaryIndex::with_capacity(legacy.len());
    let mut collisions = 0u64;

    for (pk, loc) in legacy {
        let h = xxh3_128(&canonical_bytes(&pk));
        if idx.insert_raw(h, loc).is_some() {
            collisions += 1;
        }
    }

    if collisions > 0 {
        warn!(
            "Migración v1→v2 de {:?}: {} colisiones hash detectadas \
             (esperado: 0 con xxh3_128). Revisar si hay PKs equivalentes \
             bajo NFC que antes eran distintas.",
            path, collisions
        );
    }

    info!("primary.idx v1 migrado en memoria a v2 ({:?})", path);
    Ok(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_index() -> PrimaryIndex {
        let mut idx = PrimaryIndex::new();
        idx.insert("pk_alpha", (10, 0));
        idx.insert("pk_beta", (20, 1));
        idx.insert("pk_gamma", (30, 2));
        idx
    }

    // -----------------------------------------------------------------------
    // Save/load roundtrip con header v2
    // -----------------------------------------------------------------------

    #[test]
    fn save_load_roundtrip_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("primary.idx");

        let original = make_test_index();
        save_primary_index(&path, &original).expect("save");

        // Verify header bytes
        let raw = fs::read(&path).unwrap();
        assert_eq!(&raw[..4], b"BTPI", "magic must be present");
        assert_eq!(raw[4], 2, "version must be 2");

        let loaded = load_primary_index(&path).expect("load");
        assert_eq!(loaded.len(), original.len());
        assert_eq!(loaded.get("pk_alpha"), Some((10, 0)));
        assert_eq!(loaded.get("pk_beta"), Some((20, 1)));
        assert_eq!(loaded.get("pk_gamma"), Some((30, 2)));
    }

    // -----------------------------------------------------------------------
    // Lectura de archivo v1 (con header)
    // -----------------------------------------------------------------------

    #[test]
    fn load_v1_file_with_header() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("primary.idx");

        // Write a v1 file: magic + version 1 + HashMap<String, _> payload.
        let mut raw: HashMap<String, (u64, u32)> = HashMap::new();
        raw.insert("v1_pk1".to_string(), (1, 0));
        raw.insert("v1_pk2".to_string(), (2, 1));

        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            file.write_all(&PRIMARY_IDX_MAGIC).unwrap();
            file.write_all(&[PRIMARY_IDX_VERSION_V1, 0, 0, 0]).unwrap();
            bincode::serialize_into(&mut file, &raw).unwrap();
            file.flush().unwrap();
        }

        let loaded = load_primary_index(&path).expect("load v1");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("v1_pk1"), Some((1, 0)));
        assert_eq!(loaded.get("v1_pk2"), Some((2, 1)));
    }

    // -----------------------------------------------------------------------
    // Lectura de archivo legacy (sin header)
    // -----------------------------------------------------------------------

    #[test]
    fn load_legacy_file_without_header() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("primary.idx");

        let mut raw: HashMap<String, (u64, u32)> = HashMap::new();
        raw.insert("legacy1".to_string(), (1, 0));
        raw.insert("legacy2".to_string(), (2, 1));

        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            bincode::serialize_into(&mut file, &raw).unwrap();
            file.flush().unwrap();
        }

        let loaded = load_primary_index(&path).expect("load legacy");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("legacy1"), Some((1, 0)));
        assert_eq!(loaded.get("legacy2"), Some((2, 1)));
    }

    // -----------------------------------------------------------------------
    // Version desconocida
    // -----------------------------------------------------------------------

    #[test]
    fn load_unknown_version_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("primary.idx");

        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            file.write_all(&PRIMARY_IDX_MAGIC).unwrap();
            file.write_all(&[99, 0, 0, 0]).unwrap();
            file.write_all(&[0u8; 16]).unwrap();
            file.flush().unwrap();
        }

        let result = load_primary_index(&path);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("99"), "error must mention version 99, got: {}", err_msg);
    }

    // -----------------------------------------------------------------------
    // Migración silenciosa: legacy → load → save (v2) → load
    // -----------------------------------------------------------------------

    #[test]
    fn silent_migration_legacy_to_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("primary.idx");

        // Write legacy file
        let mut raw: HashMap<String, (u64, u32)> = HashMap::new();
        raw.insert("migrated_pk".to_string(), (7, 42));
        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            bincode::serialize_into(&mut file, &raw).unwrap();
            file.flush().unwrap();
        }

        // Load (legacy → v2 in memory)
        let loaded = load_primary_index(&path).expect("load legacy");
        assert_eq!(loaded.get("migrated_pk"), Some((7, 42)));

        // Save (now v2 on disk)
        save_primary_index(&path, &loaded).expect("save v2");

        // Verify header is v2
        let raw_bytes = fs::read(&path).unwrap();
        assert_eq!(&raw_bytes[..4], b"BTPI");
        assert_eq!(raw_bytes[4], 2, "should be v2 after migration save");

        // Load again — should read v2 directly
        let reloaded = load_primary_index(&path).expect("load v2");
        assert_eq!(reloaded.get("migrated_pk"), Some((7, 42)));
    }

    // -----------------------------------------------------------------------
    // Migración v1 (con header) → v2
    // -----------------------------------------------------------------------

    #[test]
    fn migration_v1_header_to_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("primary.idx");

        // Write v1 with header
        let mut raw: HashMap<String, (u64, u32)> = HashMap::new();
        raw.insert("v1_key".to_string(), (5, 10));
        {
            let mut file = BufWriter::new(fs::File::create(&path).unwrap());
            file.write_all(&PRIMARY_IDX_MAGIC).unwrap();
            file.write_all(&[PRIMARY_IDX_VERSION_V1, 0, 0, 0]).unwrap();
            bincode::serialize_into(&mut file, &raw).unwrap();
            file.flush().unwrap();
        }

        // Load → migrate to v2 in memory
        let loaded = load_primary_index(&path).expect("load v1");
        assert_eq!(loaded.get("v1_key"), Some((5, 10)));

        // Save → v2 on disk
        save_primary_index(&path, &loaded).expect("save v2");

        // Verify
        let raw_bytes = fs::read(&path).unwrap();
        assert_eq!(raw_bytes[4], 2);

        let reloaded = load_primary_index(&path).expect("load v2");
        assert_eq!(reloaded.get("v1_key"), Some((5, 10)));
    }

    // -----------------------------------------------------------------------
    // Empty index
    // -----------------------------------------------------------------------

    #[test]
    fn save_load_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("primary.idx");

        let idx = PrimaryIndex::new();
        save_primary_index(&path, &idx).expect("save empty");

        let loaded = load_primary_index(&path).expect("load empty");
        assert!(loaded.is_empty());
    }
}
