use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use crate::core::storage::table::Table;

#[derive(Clone, Debug)]
pub struct TableUpdateEvent {
    pub entity: String,
    pub table_name: String,
    pub event_type: String, // "INSERT", "UPDATE", "DELETE", "REFRESH"
    pub pk: String,
    pub row: Vec<String>,
}

// Manejador de tablas para mantenerlas abiertas en memoria
pub struct TableManager {
    tables: RwLock<HashMap<String, Arc<RwLock<Table>>>>,
    pub events_tx: broadcast::Sender<TableUpdateEvent>,
}

impl TableManager {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1000);
        Self {
            tables: RwLock::new(HashMap::new()),
            events_tx: tx,
        }
    }

    pub fn get_table(&self, entity: &str, table_name: &str) -> anyhow::Result<Arc<RwLock<Table>>> {
        let key = format!("{}/{}", entity, table_name);
        {
            let cache = self.tables.read().unwrap();
            if let Some(table) = cache.get(&key) {
                return Ok(table.clone());
            }
        }
        let mut cache = self.tables.write().unwrap();
        if let Some(table) = cache.get(&key) {
            return Ok(table.clone());
        }
        let base_path = std::path::Path::new("data").join(entity);
        let table = Table::open(&base_path, table_name)?;
        let table_arc = Arc::new(RwLock::new(table));
        cache.insert(key, table_arc.clone());
        Ok(table_arc)
    }
}
