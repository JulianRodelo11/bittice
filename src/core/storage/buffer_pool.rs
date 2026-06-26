//! Shared mmap prefetch budget (DuckDB-style buffer pool lite).
//!
//! When `BITTICE_BUFFER_POOL_MB` is set, column prefetch (`prefetch_fields`) is skipped once
//! the tracked advisory bytes would exceed the cap. Exact-index warm is unaffected.

use std::sync::atomic::{AtomicU64, Ordering};

static PREFETCH_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn buffer_pool_limit_bytes() -> Option<u64> {
    std::env::var("BITTICE_BUFFER_POOL_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|mb| mb.saturating_mul(1024 * 1024))
}

pub fn prefetch_budget_available(estimated_bytes: u64) -> bool {
    let Some(limit) = buffer_pool_limit_bytes() else {
        return true;
    };
    if estimated_bytes == 0 {
        return true;
    }
    PREFETCH_BYTES
        .load(Ordering::Relaxed)
        .saturating_add(estimated_bytes)
        <= limit
}

pub fn record_prefetch(estimated_bytes: u64) {
    if estimated_bytes > 0 {
        PREFETCH_BYTES.fetch_add(estimated_bytes, Ordering::Relaxed);
    }
}

pub fn reset_prefetch_accounting() {
    PREFETCH_BYTES.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_blocks_when_over_limit() {
        reset_prefetch_accounting();
        std::env::set_var("BITTICE_BUFFER_POOL_MB", "1");
        assert!(prefetch_budget_available(100));
        record_prefetch(900_000);
        assert!(!prefetch_budget_available(200_000));
        std::env::remove_var("BITTICE_BUFFER_POOL_MB");
        reset_prefetch_accounting();
    }
}
