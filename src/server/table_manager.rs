use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use std::path::Path;
use crate::core::storage::table::Table;

#[derive(Debug, Clone)]
pub struct TableUpdateEvent {
    pub entity: String,
    pub table_name: String,
    pub event_type: String, // "INSERT", "UPDATE", "DELETE"
    pub pk: String,
    pub row: Vec<String>,
}

pub struct TableManager {
    pub tables: RwLock<HashMap<String, Arc<RwLock<Table>>>>,
    pub events_tx: broadcast::Sender<TableUpdateEvent>,
}

impl TableManager {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<TableUpdateEvent>(100);
        // We don't spawn the heartbeat here to avoid "no reactor" panics 
        // during sync startup.

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
                return Ok(Arc::clone(table));
            }
        }

        let entity_path = Path::new("data").join(entity);
        let table = Arc::new(RwLock::new(Table::open(&entity_path, table_name)?));
        
        let mut cache = self.tables.write().unwrap();
        cache.insert(key, Arc::clone(&table));
        Ok(table)
    }
}
