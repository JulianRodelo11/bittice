use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use crate::core::storage::table::Table;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct TableUpdateEvent {
    pub entity: String,
    pub table_name: String,
    pub event_type: String,
    pub pk: String,
    pub row: Vec<String>,
}

const DEFAULT_MAX_OPEN_TABLES: usize = 200;

pub struct TableManager {
    pub tables: RwLock<HashMap<String, Arc<RwLock<Table>>>>,
    pub events_tx: broadcast::Sender<TableUpdateEvent>,
    max_open: usize,
    open_count: AtomicUsize,
    pub dirty_tables: RwLock<HashSet<String>>,
}

impl TableManager {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<TableUpdateEvent>(100);
        let max_open = std::env::var("BITTICE_MAX_OPEN_TABLES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_OPEN_TABLES);
        Self {
            tables: RwLock::new(HashMap::new()),
            events_tx: tx,
            max_open,
            open_count: AtomicUsize::new(0),
            dirty_tables: RwLock::new(HashSet::new()),
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

        let keys: Vec<String> = cache.keys().cloned().collect();
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