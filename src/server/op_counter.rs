//! Global billable-operation counter, per-hour-bucket, with crash-safe disk
//! persistence. Reported to the control plane via heartbeat (`heartbeat.rs`).
//!
//! What's an op:
//!   - 1 unary REST/gRPC call that returns 2xx
//!   - 1 notification yielded on a gRPC SubscribeUpdates stream
//! What's NOT an op (and is never bumped here):
//!   - admin endpoints (`/_config`, `/_entities`, `/healthz`)
//!   - 4xx / 5xx responses
//!   - opening a gRPC stream (only delivered messages count)
//!
//! Storage shape (data/.request_counts.json):
//! ```json
//! {
//!   "2026-05-24T17:00:00Z": { "unary": 1832, "notification": 9421 },
//!   "2026-05-24T18:00:00Z": { "unary": 42,   "notification": 110 }
//! }
//! ```
//!
//! Concurrency: in-memory map guarded by `parking_lot::Mutex` (sub-µs locks,
//! tons of writers, low contention — atomics-per-bucket would be cleaner but
//! the bucket set is growing/shrinking with the clock so Mutex is simpler).
//! Increments are O(hash + 2 dict lookups); disk writes happen out-of-band
//! every `FSYNC_INTERVAL` from a dedicated task.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const FSYNC_INTERVAL: Duration = Duration::from_secs(30);

/// Retention on disk. Heartbeat only reports the current + previous hour
/// bucket, so any older bucket is purely "in case the control plane lost
/// the previous heartbeat and now needs catch-up." A motor that's been
/// offline >48h is in trouble for other reasons — capping here keeps the
/// file from growing unbounded if a heartbeat path is broken for days.
const ON_DISK_RETENTION_HOURS: i64 = 48;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum OpType {
    Unary,
    Notification,
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct BucketCounts {
    #[serde(default)]
    unary: u64,
    #[serde(default)]
    notification: u64,
}

impl BucketCounts {
    fn bump(&mut self, op: OpType) {
        match op {
            OpType::Unary => self.unary = self.unary.saturating_add(1),
            OpType::Notification => self.notification = self.notification.saturating_add(1),
        }
    }
}

/// Singleton: one counter for the whole process. Populated lazily on first
/// access via `instance()`.
static COUNTER: once_cell::sync::OnceCell<OpCounter> = once_cell::sync::OnceCell::new();

pub struct OpCounter {
    /// hour_iso (e.g. "2026-05-24T17:00:00Z") → counts
    buckets: Arc<Mutex<HashMap<String, BucketCounts>>>,
    disk_path: PathBuf,
}

/// Initialize the global counter. Idempotent — first caller wins. Spawns the
/// background fsync task. Safe to call once at engine startup before any
/// middleware/interceptor reaches for `instance()`.
pub fn init(data_root: &std::path::Path) {
    let _ = COUNTER.set(OpCounter::new(data_root.join(".request_counts.json")));
    if let Some(c) = COUNTER.get() {
        c.spawn_persistence_task();
    }
}

/// Get the singleton. Returns None if `init()` wasn't called (= local mode
/// without metering, e.g. unit tests). All increment/snapshot APIs no-op in
/// that case so callers don't need to branch.
pub fn instance() -> Option<&'static OpCounter> {
    COUNTER.get()
}

/// Convenience: bump the counter from anywhere. No-op when uninitialized.
pub fn bump(op: OpType) {
    if let Some(c) = instance() {
        c.bump(op);
    }
}

impl OpCounter {
    fn new(disk_path: PathBuf) -> Self {
        let buckets = match std::fs::read_to_string(&disk_path) {
            Ok(raw) => match serde_json::from_str::<HashMap<String, BucketCounts>>(&raw) {
                Ok(map) => {
                    debug!(
                        "op_counter: restored {} bucket(s) from {}",
                        map.len(),
                        disk_path.display()
                    );
                    map
                }
                Err(e) => {
                    warn!(
                        "op_counter: ignoring corrupt {}: {e:#}",
                        disk_path.display()
                    );
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };

        Self {
            buckets: Arc::new(Mutex::new(buckets)),
            disk_path,
        }
    }

    pub fn bump(&self, op: OpType) {
        let bucket = current_bucket_iso();
        let mut guard = self.buckets.lock();
        guard.entry(bucket).or_default().bump(op);
    }

    /// Snapshot of the current + previous hour buckets, ready to ship over the
    /// heartbeat extra blob. Buckets older than that are still in memory (and
    /// will be persisted next fsync) but are not re-sent — the control plane
    /// already has them from prior heartbeats.
    pub fn heartbeat_snapshot(&self) -> serde_json::Value {
        let guard = self.buckets.lock();
        let cur = current_bucket_iso();
        let prev = previous_bucket_iso();
        let mut out = serde_json::Map::new();
        for key in [&cur, &prev] {
            if let Some(bc) = guard.get(key) {
                out.insert(
                    key.clone(),
                    serde_json::json!({
                        "unary": bc.unary,
                        "notification": bc.notification,
                    }),
                );
            }
        }
        serde_json::Value::Object(out)
    }

    fn spawn_persistence_task(&self) {
        let buckets = self.buckets.clone();
        let path = self.disk_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(FSYNC_INTERVAL).await;
                let snapshot = {
                    let mut guard = buckets.lock();
                    drop_old_buckets(&mut guard);
                    guard.clone()
                };
                if let Err(e) = persist(&path, &snapshot) {
                    warn!("op_counter: persist failed: {e:#}");
                }
            }
        });
    }
}

fn drop_old_buckets(buckets: &mut HashMap<String, BucketCounts>) {
    let cutoff = Utc::now() - chrono::Duration::hours(ON_DISK_RETENTION_HOURS);
    buckets.retain(|k, _| {
        DateTime::parse_from_rfc3339(k)
            .map(|t| t.with_timezone(&Utc) >= cutoff)
            .unwrap_or(false)
    });
}

fn persist(path: &std::path::Path, data: &HashMap<String, BucketCounts>) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    // Atomic write via temp + rename so a crash mid-write can't corrupt the file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn current_bucket_iso() -> String {
    bucket_iso(Utc::now())
}

fn previous_bucket_iso() -> String {
    bucket_iso(Utc::now() - chrono::Duration::hours(1))
}

fn bucket_iso(t: DateTime<Utc>) -> String {
    let hour_start = Utc
        .with_ymd_and_hms(t.year(), t.month(), t.day(), t.hour(), 0, 0)
        .single()
        .unwrap_or(t);
    // RFC3339 with explicit Z suffix to match the control plane parser.
    hour_start.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_iso_truncates_to_hour() {
        let t = Utc.with_ymd_and_hms(2026, 5, 24, 17, 43, 21).unwrap();
        assert_eq!(bucket_iso(t), "2026-05-24T17:00:00Z");
    }

    #[test]
    fn bump_then_snapshot_returns_counts() {
        let counter = OpCounter::new(std::env::temp_dir().join("op_counter_test.json"));
        counter.bump(OpType::Unary);
        counter.bump(OpType::Unary);
        counter.bump(OpType::Notification);
        let snap = counter.heartbeat_snapshot();
        let cur = current_bucket_iso();
        let bucket = snap.get(&cur).unwrap();
        assert_eq!(bucket.get("unary").unwrap().as_u64().unwrap(), 2);
        assert_eq!(bucket.get("notification").unwrap().as_u64().unwrap(), 1);
    }

    #[test]
    fn drop_old_buckets_removes_stale_entries() {
        let mut buckets = HashMap::new();
        buckets.insert(bucket_iso(Utc::now()), BucketCounts { unary: 1, notification: 0 });
        buckets.insert(
            bucket_iso(Utc::now() - chrono::Duration::hours(72)),
            BucketCounts { unary: 99, notification: 0 },
        );
        drop_old_buckets(&mut buckets);
        assert_eq!(buckets.len(), 1);
    }
}
