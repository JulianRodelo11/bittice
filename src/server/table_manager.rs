use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use crate::core::storage::table::Table;

// Manejador de tablas para mantenerlas abiertas en memoria
pub struct TableManager {
    tables: RwLock<HashMap<String, Arc<RwLock<Table>>>>,
}

impl TableManager {
    pub fn new() -> Self {
        Self {
            tables: RwLock::new(HashMap::new()),
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
