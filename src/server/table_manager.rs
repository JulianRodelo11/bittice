use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use crate::core::storage::table::Table;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct TableUpdateEvent {
    pub entity: String,
    pub table_name: String,
    pub event_type: String,
    pub pk: String,
    pub row: Vec<String>,
}

/// A mirror table directory is counted if it contains `manifest.json` or at least one `*.dat`
/// under `segments/*/`.
fn is_valid_mirror_table_dir(dir: &std::path::Path) -> bool {
    if dir.join("manifest.json").is_file() {
        return true;
    }
    let seg_root = dir.join("segments");
    let Ok(rd) = fs::read_dir(&seg_root) else {
        return false;
    };
    for seg in rd.flatten() {
        let p = seg.path();
        if !p.is_dir() {
            continue;
        }
        let Ok(rd2) = fs::read_dir(&p) else {
            continue;
        };
        for f in rd2.flatten() {
            if f
                .path()
                .extension()
                .map(|e| e.eq_ignore_ascii_case("dat"))
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

fn count_mirror_tables_on_disk() -> usize {
    let mut n = 0usize;
    for entity_path in crate::core::data_paths::iter_mirror_entity_paths() {
        let Ok(rd) = fs::read_dir(&entity_path) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && is_valid_mirror_table_dir(&p) {
                n += 1;
            }
        }
    }
    n
}

pub struct TableManager {
    pub tables: RwLock<HashMap<String, Arc<RwLock<Table>>>>,
    pub events_tx: broadcast::Sender<TableUpdateEvent>,
    max_open: usize,
    open_count: AtomicUsize,
    pub dirty_tables: RwLock<HashSet<String>>,
    /// When set, `evict_lru` closes tables **not** in this set first (from `.bittice_ops.json`).
    query_priority_keys: Arc<RwLock<Option<Arc<HashSet<String>>>>>,
}

fn ops_table_cache_priority_enabled() -> bool {
    match std::env::var("BITTICE_OPS_TABLE_CACHE_PRIORITY") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            !matches!(t.as_str(), "0" | "false" | "no" | "off")
        }
        Err(_) => true,
    }
}

impl TableManager {
    /// Maximum number of mirrored `Table` handles kept open in memory.
    ///
    /// If `BITTICE_MAX_OPEN_TABLES` is set, that value wins. Otherwise the limit is
    /// `max(10, ceil(total × (1 + MARGIN/100)))` where `total` is the number of valid table
    /// directories under all mirror entity roots, and MARGIN is `BITTICE_MAX_OPEN_TABLES_MARGIN_PCT`
    /// (default 20, clamped 0–200).
    ///
    /// This is computed once at startup (`TableManager::new`); there is no periodic refresh loop
    /// in this module—restart the process or set `BITTICE_MAX_OPEN_TABLES` to change the cap later.
    fn compute_dynamic_max_open() -> usize {
        if let Ok(v) = std::env::var("BITTICE_MAX_OPEN_TABLES") {
            let t = v.trim();
            if let Ok(n) = t.parse::<usize>() {
                return n.max(1);
            }
        }
        let total = count_mirror_tables_on_disk();
        let margin_pct: u32 = std::env::var("BITTICE_MAX_OPEN_TABLES_MARGIN_PCT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(20)
            .min(200);
        let num = total as u128;
        let pct = margin_pct as u128;
        let scaled = num.saturating_mul(100 + pct).div_ceil(100);
        scaled.max(10).min(usize::MAX as u128) as usize
    }

    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<TableUpdateEvent>(100);
        let max_open = Self::compute_dynamic_max_open();
        Self {
            tables: RwLock::new(HashMap::new()),
            events_tx: tx,
            max_open,
            open_count: AtomicUsize::new(0),
            dirty_tables: RwLock::new(HashSet::new()),
            query_priority_keys: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_query_priority_keys(&self, keys: Option<Arc<HashSet<String>>>) {
        *self.query_priority_keys.write().unwrap() = keys;
    }

    /// Reload `.bittice_ops.json` and refresh priority keys (same filter as HTTP when applicable).
    pub fn refresh_query_priority_keys_from_ops(&self, entity_filter: Option<String>) {
        if !ops_table_cache_priority_enabled() {
            self.set_query_priority_keys(None);
            return;
        }
        match crate::core::saved_queries::load_operations_with_filter(entity_filter) {
            Ok(ops) => {
                let keys = crate::core::saved_queries::collect_ops_query_table_keys(&ops);
                if keys.is_empty() {
                    self.set_query_priority_keys(None);
                } else {
                    debug!(
                        "TableManager: {} query-priority table key(s) from saved operations",
                        keys.len()
                    );
                    self.set_query_priority_keys(Some(Arc::new(keys)));
                }
            }
            Err(e) => {
                warn!("refresh_query_priority_keys_from_ops: {}", e);
            }
        }
    }

    pub fn get_table(&self, entity: &str, table_name: &str) -> anyhow::Result<Arc<RwLock<Table>>> {
        let key = format!("{}/{}", entity, table_name);
        {
            let cache = self.tables.read().unwrap();
            if let Some(table) = cache.get(&key) {
                return Ok(Arc::clone(table));
            }
        }

        if self.open_count.load(Ordering::Relaxed) >= self.max_open {
            self.evict_lru();
        }

        let entity_path = crate::core::data_paths::mirror_entity_dir(entity);
        let table = Arc::new(RwLock::new(
            Table::open(&entity_path, table_name).with_context(|| {
                format!(
                    "mirror Table::open entity_dir={:?} table_dir={}",
                    entity_path.display(),
                    table_name
                )
            })?,
        ));
        self.open_count.fetch_add(1, Ordering::Relaxed);

        let mut cache = self.tables.write().unwrap();
        if let Some(existing) = cache.get(&key) {
            self.open_count.fetch_sub(1, Ordering::Relaxed);
            return Ok(Arc::clone(existing));
        }
        cache.insert(key, Arc::clone(&table));
        Ok(table)
    }

    pub fn mark_dirty(&self, entity: &str, table_name: &str) {
        let key = format!("{}/{}", entity, table_name);
        self.dirty_tables.write().unwrap().insert(key);
    }

    pub fn flush_dirty_tables(&self) {
        let keys: Vec<String> = {
            let mut dirty = self.dirty_tables.write().unwrap();
            dirty.drain().collect()
        };
        if keys.is_empty() {
            return;
        }
        let cache = self.tables.read().unwrap();
        for key in &keys {
            if let Some(table_arc) = cache.get(key) {
                if let Ok(mut t) = table_arc.write() {
                    let _ = t.flush_active_segment();
                }
            }
        }
        debug!("TableManager: flushed {} dirty table(s)", keys.len());
    }

    fn evict_lru(&self) {
        let target = if self.max_open > 50 {
            self.max_open / 2
        } else {
            self.max_open.saturating_sub(1)
        };
        let to_evict = self
            .open_count
            .load(Ordering::Relaxed)
            .saturating_sub(target);
        if to_evict == 0 {
            return;
        }

        let mut cache = self.tables.write().unwrap();
        if cache.is_empty() {
            return;
        }

        let mut keys: Vec<String> = cache.keys().cloned().collect();
        if let Some(hot) = self.query_priority_keys.read().unwrap().as_ref() {
            let (mut cold, warm): (Vec<String>, Vec<String>) = keys
                .into_iter()
                .partition(|k| !hot.contains(k.as_str()));
            cold.extend(warm);
            keys = cold;
        }
        let evict_count = to_evict.min(keys.len());
        debug!(
            "TableManager: evicting {} of {} cached tables (open_limit={})",
            evict_count,
            keys.len(),
            self.max_open
        );

        let mut evicted = 0usize;
        for key in &keys {
            if evicted >= evict_count {
                break;
            }
            let removed = cache.remove(key);
            if removed.is_some() {
                self.open_count.fetch_sub(1, Ordering::Relaxed);
                evicted += 1;
                if let Some(table_lock) = removed {
                    if let Ok(mut table) = table_lock.write() {
                        let _ = table.close();
                    }
                }
            }
        }
    }

    pub fn close_table(&self, entity: &str, table_name: &str) {
        let key = format!("{}/{}", entity, table_name);
        let mut cache = self.tables.write().unwrap();
        if let Some(table_lock) = cache.remove(&key) {
            self.open_count.fetch_sub(1, Ordering::Relaxed);
            drop(cache);
            if let Ok(mut table) = table_lock.write() {
                let _ = table.close();
            }
            debug!("TableManager: closed table '{}'", key);
        }
    }

    pub fn open_table_count(&self) -> usize {
        self.open_count.load(Ordering::Relaxed)
    }
}