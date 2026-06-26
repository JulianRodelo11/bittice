//! Limits concurrent multi-table join queries so overlapping heavy reads
//! (e.g. split_enrichment) cannot pile up and saturate CPU on small hosts.
//!
//! `BITTICE_MAX_CONCURRENT_JOIN_QUERIES` (default 2)

use parking_lot::{Condvar, Mutex};
use std::sync::LazyLock;

struct JoinGate {
    active: Mutex<usize>,
    cv: Condvar,
    max: usize,
}

static JOIN_GATE: LazyLock<JoinGate> = LazyLock::new(|| JoinGate {
    active: Mutex::new(0),
    cv: Condvar::new(),
    max: std::env::var("BITTICE_MAX_CONCURRENT_JOIN_QUERIES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(2),
});

struct JoinPermit;

impl Drop for JoinPermit {
    fn drop(&mut self) {
        let mut active = JOIN_GATE.active.lock();
        *active = active.saturating_sub(1);
        JOIN_GATE.cv.notify_one();
    }
}

/// Blocks until a join-query slot is available. Held for the duration of
/// `execute_join_query`.
pub fn acquire_join_permit() -> impl Drop {
    let mut active = JOIN_GATE.active.lock();
    while *active >= JOIN_GATE.max {
        JOIN_GATE.cv.wait(&mut active);
    }
    *active += 1;
    JoinPermit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_join_permit() {
        let _p = acquire_join_permit();
    }
}
