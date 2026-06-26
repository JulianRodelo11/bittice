//! Short-TTL response cache for expensive saved read ops.
//!
//! Prevents duplicate work when clients retry on timeout or fire overlapping
//! requests for the same placa. Configure via:
//!   BITTICE_RESPONSE_CACHE_TTL_SECS (default 0 = disabled)
//!   BITTICE_RESPONSE_CACHE_OPS (comma-separated op names)
//!   BITTICE_RESPONSE_CACHE_MAX_ENTRIES (default 256)

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

pub struct ResponseCache {
    ttl: Duration,
    ops: HashSet<String>,
    max_entries: usize,
    store: Mutex<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    body: Value,
    inserted: Instant,
}

impl ResponseCache {
    pub fn from_env() -> Self {
        let ttl_secs: u64 = std::env::var("BITTICE_RESPONSE_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let max_entries: usize = std::env::var("BITTICE_RESPONSE_CACHE_MAX_ENTRIES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(256);
        let ops: HashSet<String> = std::env::var("BITTICE_RESPONSE_CACHE_OPS")
            .ok()
            .map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            ttl: Duration::from_secs(ttl_secs),
            ops,
            max_entries,
            store: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero() && !self.ops.is_empty()
    }

    pub fn is_cacheable_op(&self, op_name: &str) -> bool {
        self.enabled() && self.ops.contains(op_name)
    }

    pub fn get(&self, op_name: &str, params: &HashMap<String, String>) -> Option<Value> {
        if !self.is_cacheable_op(op_name) {
            return None;
        }
        let key = cache_key(op_name, params);
        let mut store = self.store.lock().unwrap();
        let entry = store.get(&key)?;
        if entry.inserted.elapsed() > self.ttl {
            store.remove(&key);
            return None;
        }
        Some(entry.body.clone())
    }

    pub fn put(&self, op_name: &str, params: &HashMap<String, String>, body: Value) {
        if !self.is_cacheable_op(op_name) {
            return;
        }
        let key = cache_key(op_name, params);
        let mut store = self.store.lock().unwrap();
        if store.len() >= self.max_entries {
            // Drop oldest entry — bounded memory for hot placas.
            if let Some(oldest_key) = store
                .iter()
                .min_by_key(|(_, v)| v.inserted)
                .map(|(k, _)| k.clone())
            {
                store.remove(&oldest_key);
            }
        }
        store.insert(
            key,
            CacheEntry {
                body,
                inserted: Instant::now(),
            },
        );
    }

    pub fn clear(&self) {
        self.store.lock().unwrap().clear();
    }
}

fn cache_key(op_name: &str, params: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&String, &String)> = params.iter().collect();
    pairs.sort_by_key(|(k, _)| *k);
    let qs: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();
    format!("{}?{}", op_name, qs.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_for_param_order() {
        let mut a = HashMap::new();
        a.insert("placa".into(), "ABC123".into());
        a.insert("page".into(), "1".into());
        let mut b = HashMap::new();
        b.insert("page".into(), "1".into());
        b.insert("placa".into(), "ABC123".into());
        assert_eq!(cache_key("op", &a), cache_key("op", &b));
    }
}
