//! CDC mirror durability vs throughput tuning (single source of truth for env vars).
//!
//! ## `BITTICE_CDC_DURABILITY`
//! - **`strict`**: mirror segment flush every ROW batch + WAL fsync after each op. Slowest; closest to
//!   “fsync-level” local safety between segment rotations (still not a full distributed transaction).
//! - **`balanced`** (default): batched segment flush (32 batches/table) + buffered WAL (flushed on segment rotate).
//! - **`throughput`** / **`fast`**: larger batches (128); same WAL behavior as balanced.
//!
//! Overrides:
//! - **`BITTICE_CDC_MIRROR_FLUSH_EVERY_N`**: positive integer wins over profile defaults.
//! - **`BITTICE_CDC_WAL_FSYNC_EACH_OP`**: `1`/`true` forces `flush()` after every WAL append (like strict WAL).

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdcDurability {
    Strict,
    Balanced,
    Throughput,
}

fn parse_durability_env() -> CdcDurability {
    match std::env::var("BITTICE_CDC_DURABILITY") {
        Ok(s) => match s.trim().to_ascii_lowercase().as_str() {
            "strict" => CdcDurability::Strict,
            "throughput" | "fast" => CdcDurability::Throughput,
            _ => CdcDurability::Balanced,
        },
        Err(_) => CdcDurability::Balanced,
    }
}

/// Resolved once per process.
pub fn durability() -> CdcDurability {
    static D: OnceLock<CdcDurability> = OnceLock::new();
    *D.get_or_init(parse_durability_env)
}

/// ROW batches per mirror table between `Table::flush_active_segment` calls.
pub fn mirror_flush_every_n_batches() -> u32 {
    if let Ok(s) = std::env::var("BITTICE_CDC_MIRROR_FLUSH_EVERY_N") {
        if let Ok(n) = s.trim().parse::<u32>() {
            if n > 0 {
                return n;
            }
        }
    }
    match durability() {
        CdcDurability::Strict => 1,
        CdcDurability::Balanced => 32,
        CdcDurability::Throughput => 128,
    }
}

pub fn wal_sync_each_append() -> bool {
    if std::env::var("BITTICE_CDC_WAL_FSYNC_EACH_OP")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
    {
        return true;
    }
    matches!(durability(), CdcDurability::Strict)
}
