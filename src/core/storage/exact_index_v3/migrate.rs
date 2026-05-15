//! Migration from v1/v2 exact index files to v3 format.

use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use super::format::{EXACT_IDX_MAGIC, EXACT_IDX_VERSION_V3};
use super::reader::SnapshotReader;
use super::writer::write_exact_index_v3_from_hashmap;
use crate::core::storage::exact_index_io::load_exact_index;

/// Statistics returned by [`migrate_exact_index_to_v3`].
pub struct MigrationStats {
    /// Number of hash entries in the migrated index.
    pub num_entries: usize,
    /// File size before migration (bytes). 0 if already v3.
    pub old_size: u64,
    /// File size after migration (bytes). 0 if already v3.
    pub new_size: u64,
    /// Wall-clock time for the migration. Near-zero if already v3.
    pub elapsed: std::time::Duration,
    /// True if the file was already v3 and no write was performed.
    pub already_v3: bool,
}

/// Migrate an exact index file at `path` to v3 format in-place.
///
/// # Early exit
///
/// If the file already starts with `BTXI` + version 3, the function returns
/// immediately without reading or writing anything.
///
/// # Algorithm
///
/// 1. Peek at the first 5 bytes to detect the existing format.
/// 2. If already v3 → return early with `already_v3 = true`.
/// 3. Load via [`load_exact_index`] which handles v1, v2, and legacy formats.
/// 4. Extract the inner `HashMap<u128, _>` via `into_inner()`.
/// 5. Write a new v3 file over `path` (atomic rename via `.tmp`).
/// 6. Return statistics.
pub fn migrate_exact_index_to_v3(path: &Path) -> Result<MigrationStats> {
    let start = Instant::now();

    // Peek at the first 5 bytes to detect the format without opening the full file.
    let header_bytes = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)
            .map_err(|e| anyhow::anyhow!("failed to open {:?}: {}", path, e))?;
        let mut buf = [0u8; 5];
        let n = f.read(&mut buf).map_err(|e| anyhow::anyhow!("read {:?}: {}", path, e))?;
        (buf, n)
    };

    let (buf, n) = header_bytes;

    // Check: magic == BTXI and version == 3
    if n >= 5 && buf[0..4] == EXACT_IDX_MAGIC && buf[4] == EXACT_IDX_VERSION_V3 {
        return Ok(MigrationStats {
            num_entries: 0,
            old_size: 0,
            new_size: 0,
            elapsed: start.elapsed(),
            already_v3: true,
        });
    }

    // Get old file size for stats.
    let old_size = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Load the existing file (handles v1, v2, legacy).
    let exact_index = load_exact_index(path)
        .map_err(|e| anyhow::anyhow!("load_exact_index {:?}: {}", path, e))?;

    let num_entries = exact_index.len();

    // Extract the inner HashMap<u128, _>.
    let map = exact_index.into_inner();

    // Write v3 in-place.
    write_exact_index_v3_from_hashmap(path, map)
        .map_err(|e| anyhow::anyhow!("write_exact_index_v3 {:?}: {}", path, e))?;

    // Get new file size for stats.
    let new_size = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Validate the written file (early sanity check).
    let _reader = SnapshotReader::open(path)
        .map_err(|e| anyhow::anyhow!("post-migration open {:?}: {}", path, e))?;

    Ok(MigrationStats {
        num_entries,
        old_size,
        new_size,
        elapsed: start.elapsed(),
        already_v3: false,
    })
}
