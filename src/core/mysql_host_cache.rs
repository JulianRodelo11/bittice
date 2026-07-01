//! MySQL host_cache / error 1129 helpers.
//!
//! RDS can block a client IP after repeated failed connects (`ERROR 1129`). A Docker
//! restart loop makes that worse — each attempt counts against `max_connect_errors`.
//! This module classifies those errors, optionally invokes a VPC Lambda to
//! `TRUNCATE performance_schema.host_cache`, and provides backoff hints.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// Returns true when MySQL rejected the client because the host is blocked (1129).
pub fn is_mysql_host_blocked_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("(1129)")
        || lower.contains("error 1129")
        || lower.contains("blocked because of many connection errors")
        || lower.contains("flush-hosts")
        || lower.contains("flush hosts")
}

/// Transient connect errors that are worth retrying (including 1129).
pub fn is_transient_mysql_connect_error(msg: &str) -> bool {
    if is_mysql_host_blocked_error(msg) {
        return true;
    }
    let lower = msg.to_ascii_lowercase();
    lower.contains("connection timed out")
        || lower.contains("connection timeout")
        || lower.contains("timed out")
        || lower.contains("connection refused")
        || lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("server has gone away")
        || lower.contains("too many connections")
        || lower.contains("os error 22")
        || lower.contains("invalid argument")
        || lower.contains("network is unreachable")
        || lower.contains("dns")
}

/// Backoff before the next connect attempt (attempt is 1-based).
pub fn connect_backoff_secs(attempt: u32, host_blocked: bool) -> u64 {
    if host_blocked {
        // Longer waits when RDS has blocked us — avoid making 1129 worse.
        const BLOCKED: [u64; 12] = [30, 60, 120, 180, 300, 300, 600, 600, 900, 900, 1200, 1200];
        let idx = (attempt.saturating_sub(1) as usize).min(BLOCKED.len() - 1);
        return BLOCKED[idx];
    }
    const NORMAL: [u64; 12] = [2, 5, 10, 15, 30, 45, 60, 90, 120, 180, 300, 300];
    let idx = (attempt.saturating_sub(1) as usize).min(NORMAL.len() - 1);
    NORMAL[idx]
}

fn flush_cooldown() -> Duration {
    Duration::from_secs(
        std::env::var("BITTICE_OPS_FLUSH_COOLDOWN_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600),
    )
}

static LAST_FLUSH: Mutex<Option<Instant>> = Mutex::new(None);

/// Invoke the optional VPC Lambda (`BITTICE_OPS_FLUSH_URL` + `BITTICE_OPS_FLUSH_SECRET`)
/// to truncate `performance_schema.host_cache`. Rate-limited by cooldown.
pub async fn maybe_flush_host_cache() {
    let url = match std::env::var("BITTICE_OPS_FLUSH_URL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    let secret = std::env::var("BITTICE_OPS_FLUSH_SECRET").unwrap_or_default();
    let cooldown = flush_cooldown();

    {
        let mut guard = LAST_FLUSH.lock().unwrap();
        if guard
            .map(|t| t.elapsed() < cooldown)
            .unwrap_or(false)
        {
            return;
        }
        *guard = Some(Instant::now());
    }

    info!(
        "CDC: invoking host_cache flush Lambda (cooldown {}s) after MySQL 1129 / blocked host…",
        cooldown.as_secs()
    );

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("CDC: flush Lambda HTTP client build failed: {e:#}");
            return;
        }
    };

    let mut req = client.post(&url);
    if !secret.trim().is_empty() {
        req = req.header("X-Bittice-Flush-Secret", secret.trim());
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            info!("CDC: host_cache flush Lambda OK (HTTP {}).", resp.status());
        }
        Ok(resp) => {
            warn!(
                "CDC: host_cache flush Lambda returned HTTP {}.",
                resp.status()
            );
        }
        Err(e) => {
            warn!("CDC: host_cache flush Lambda request failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_1129_variants() {
        assert!(is_mysql_host_blocked_error(
            "ERROR HY000 (1129): Host '172.31.32.63' is blocked because of many connection errors"
        ));
        assert!(is_mysql_host_blocked_error(
            "unblock with 'mysqladmin flush-hosts'"
        ));
        assert!(!is_mysql_host_blocked_error("access denied for user"));
    }

    #[test]
    fn blocked_backoff_is_longer_than_normal() {
        assert!(connect_backoff_secs(1, true) >= connect_backoff_secs(1, false));
        assert!(connect_backoff_secs(3, true) >= 120);
    }
}
