//! Migration from v1/v2 exact index files to v3 format.

use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use super::format::{EXACT_IDX_MAGIC, EXACT_IDX_VERSION_V3};
use super::reader::SnapshotReader;
use crate::core::storage::exact_index::ExactIndex;

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
pub fn migrate_exact_index_to_v3(path: &Path) -> Result<MigrationStats> {
    let start = Instant::now();

    let header_bytes = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)
            .map_err(|e| anyhow::anyhow!("failed to open {:?}: {}", path, e))?;
        let mut buf = [0u8; 5];
        let n = f.read(&mut buf).map_err(|e| anyhow::anyhow!("read {:?}: {}", path, e))?;
        (buf, n)
    };

    let (buf, n) = header_bytes;

    if n >= 5 && buf[0..4] == EXACT_IDX_MAGIC && buf[4] == EXACT_IDX_VERSION_V3 {
        return Ok(MigrationStats {
            num_entries: 0,
            old_size: 0,
            new_size: 0,
            elapsed: start.elapsed(),
            already_v3: true,
        });
    }

    let old_size = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(0);

    let mut exact_index = ExactIndex::open(path)
        .map_err(|e| anyhow::anyhow!("ExactIndex::open {:?}: {}", path, e))?;

    let num_entries = exact_index.len();

    exact_index
        .save(Some(path))
        .map_err(|e| anyhow::anyhow!("ExactIndex::save {:?}: {}", path, e))?;

    let new_size = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(0);

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
