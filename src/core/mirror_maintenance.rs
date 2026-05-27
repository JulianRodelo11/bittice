//! Offline mirror maintenance (compact, reconcile). Used by CLI one-shot containers.

use anyhow::{Context, Result};
use tracing::info;

use crate::core::data_paths;
use crate::core::storage::table::Table;

/// Open a mirror table, reconcile tombstones, and compact segments. Live counts are
/// derived from each segment's deleted.bitmap on demand, not stored in the manifest,
/// so there is no cache to keep in sync.
pub fn compact_mirror_table(entity: &str, table_name: &str) -> Result<usize> {
    let entity_path = data_paths::mirror_entity_dir(entity);
    let mut table = Table::open(&entity_path, table_name)
        .with_context(|| format!("open mirror {entity}/{table_name}"))?;
    table.reconcile_orphan_rows()?;
    let before = table.immutable_segment_count();
    let removed = table.compact()?;
    info!(
        "compact_mirror_table {entity}/{table_name}: live_rows={} segments_before={} segments_removed={}",
        table.live_row_count(),
        before,
        removed
    );
    Ok(removed)
}

/// Compact every table directory under `data/mirror/<entity>/` that has a manifest.
pub fn compact_mirror_entity(entity: &str) -> Result<Vec<(String, usize)>> {
    let entity_path = data_paths::mirror_entity_dir(entity);
    let mut out = Vec::new();
    if !entity_path.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&entity_path)?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().join("manifest.json").is_file() {
            continue;
        }
        match compact_mirror_table(entity, &name) {
            Ok(n) => out.push((name, n)),
            Err(e) => {
                tracing::warn!("compact_mirror_entity skip {entity}/{name}: {e:#}");
            }
        }
    }
    Ok(out)
}
