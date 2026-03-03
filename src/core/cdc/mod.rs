use anyhow::{Context, Result};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, Pool, BinlogStreamRequest};
use mysql_common::binlog::events::{EventData, RowsEventData, TableMapEvent};
use mysql_common::row::Row;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio_stream::StreamExt;
use crate::server::table_manager::TableManager;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CdcState {
    pub binlog_file: String,
    pub binlog_pos: u32,
    pub bootstrapped_tables: Vec<String>,
    #[serde(default)]
    pub pk_map: HashMap<String, String>,
}

pub struct CdcWorker {
    url: String,
    entity: String,
    database: String,
    state_path: String,
    table_manager: Arc<TableManager>,
    column_maps: Arc<RwLock<HashMap<String, Vec<String>>>>,
    table_map_events: Arc<RwLock<HashMap<u64, TableMapEvent<'static>>>>,
}

impl CdcWorker {
    pub fn new(url: String, entity: String, database: String) -> Self {
        let state_path = format!("data/{}/cdc_state.json", entity);
        Self { 
            url, 
            entity, 
            database,
            state_path,
            table_manager: Arc::new(TableManager::new()),
            column_maps: Arc::new(RwLock::new(HashMap::new())),
            table_map_events: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn load_state(&self) -> CdcState {
        if let Ok(file) = std::fs::File::open(&self.state_path) {
            serde_json::from_reader(file).unwrap_or(CdcState {
                binlog_file: String::new(),
                binlog_pos: 4,
                bootstrapped_tables: Vec::new(),
                pk_map: HashMap::new(),
            })
        } else {
            CdcState {
                binlog_file: String::new(),
                binlog_pos: 4,
                bootstrapped_tables: Vec::new(),
                pk_map: HashMap::new(),
            }
        }
    }

    fn save_state(&self, state: &CdcState) -> Result<()> {
        let path = Path::new(&self.state_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(file, state)?;
        Ok(())
    }

    async fn fetch_all_tables(&self, conn: &mut Conn) -> Result<Vec<String>> {
        let tables: Vec<String> = conn.query("SHOW TABLES").await?;
        Ok(tables)
    }

    async fn fetch_column_names(&self, conn: &mut Conn, table_name: &str) -> Result<Vec<String>> {
        let rows: Vec<(String, String, String, String, Option<String>, String)> = 
            conn.query(format!("DESCRIBE {}", table_name)).await?;
        
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    async fn bootstrap_table(&self, conn: &mut Conn, table_name: &str, state: &mut CdcState) -> Result<()> {
        println!("CDC: Bootstrapping table '{}'...", table_name);
        
        let cols = self.fetch_column_names(conn, table_name).await?;
        {
            let mut maps = self.column_maps.write().unwrap();
            maps.insert(table_name.to_string(), cols.clone());
        }

        let table_lock = self.table_manager.get_table(&self.entity, table_name)?;
        let mut table = table_lock.write().unwrap();

        // Configurar PK real desde MySQL
        let pk_query = format!(
            "SELECT COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}' \
             AND INDEX_NAME = 'PRIMARY' ORDER BY SEQ_IN_INDEX LIMIT 1",
            self.database, table_name
        );
        let pk_col: Option<String> = conn.query_first(pk_query).await?;
        
        if let Some(col) = pk_col {
            table.manifest.primary_key = col.clone();
            state.pk_map.insert(table_name.to_string(), col);
            println!("CDC: Detected PK='{}' for table '{}'", table.manifest.primary_key, table_name);
        } else {
            if let Some(pk_cand) = cols.iter().find(|c| c.ends_with("_id") || *c == "id") {
                table.manifest.primary_key = pk_cand.clone();
                state.pk_map.insert(table_name.to_string(), pk_cand.clone());
            }
        }

        let mut result_set = conn.query_iter(format!("SELECT * FROM {}", table_name)).await?;
        let mut count = 0;

        while let Some(row) = result_set.next().await? {
            let mut data = HashMap::new();
            for (i, col_name) in cols.iter().enumerate() {
                let val: mysql_common::Value = row.get(i).unwrap_or(mysql_common::Value::NULL);
                let mut val_str = match val {
                    mysql_common::Value::NULL => "".to_string(),
                    mysql_common::Value::Bytes(ref b) => String::from_utf8_lossy(b).to_string(),
                    mysql_common::Value::Date(y, m, d, h, min, s, ms) => format!("{}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}", y, m, d, h, min, s, ms),
                    _ => format!("{:?}", val),
                };
                if val_str.starts_with("Int(") || val_str.starts_with("UInt(") {
                    if let Some(start) = val_str.find('(') {
                        if let Some(end) = val_str.find(')') {
                            val_str = val_str[start+1..end].to_string();
                        }
                    }
                }
                data.insert(col_name.clone(), val_str);
            }
            table.insert(data)?;
            count += 1;
        }

        table.flush_active_segment()?;
        println!("CDC: Table '{}' bootstrapped successfully ({} rows).", table_name, count);
        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        let opts = Opts::from_url(&self.url)?;
        let pool = Pool::new(opts);
        let mut conn = pool.get_conn().await?;

        conn.query_drop(format!("USE {}", self.database)).await?;

        let mut state = self.load_state();
        let tables = self.fetch_all_tables(&mut conn).await?;

        for table_name in &tables {
            if !state.bootstrapped_tables.contains(table_name) {
                self.bootstrap_table(&mut conn, table_name, &mut state).await?;
                state.bootstrapped_tables.push(table_name.clone());
                self.save_state(&state)?;
            } else {
                let cols = self.fetch_column_names(&mut conn, table_name).await?;
                let mut maps = self.column_maps.write().unwrap();
                maps.insert(table_name.to_string(), cols);
            }
        }

        if state.binlog_file.is_empty() {
            let row: Option<(String, u32, String, String, String)> = conn.query_first("SHOW MASTER STATUS").await?;
            if let Some((file, pos, _, _, _)) = row {
                state.binlog_file = file;
                state.binlog_pos = pos;
            }
        }

        println!("CDC: Resuming live stream from {}:{}", state.binlog_file, state.binlog_pos);

        let request = BinlogStreamRequest::new(1337)
            .with_filename(state.binlog_file.as_bytes())
            .with_pos(state.binlog_pos as u64);
            
        let mut stream = conn.get_binlog_stream(request).await?;
        let mut last_flush = std::time::Instant::now();

        while let Some(event) = stream.next().await {
            let event = event?;
            let header = event.header();
            let next_pos = header.log_pos();
            
            if next_pos > 0 {
                state.binlog_pos = next_pos;
            }

            let data = event.read_data()?;
            match data {
                Some(EventData::TableMapEvent(tm)) => {
                    let mut map = self.table_map_events.write().unwrap();
                    map.insert(tm.table_id(), tm.into_owned());
                }
                Some(EventData::RowsEvent(rows_data)) => {
                    self.handle_rows_event(rows_data, &state)?;
                }
                Some(EventData::RotateEvent(rotate_data)) => {
                    state.binlog_file = rotate_data.name().to_string();
                    state.binlog_pos = rotate_data.position() as u32;
                }
                _ => {}
            }

            self.save_state(&state)?;

            if last_flush.elapsed() > std::time::Duration::from_secs(10) {
                last_flush = std::time::Instant::now();
            }
        }

        Ok(())
    }

    fn handle_rows_event(&self, rows_data: RowsEventData, state: &CdcState) -> Result<()> {
        let table_id = match &rows_data {
            RowsEventData::WriteRowsEvent(ev) => ev.table_id(),
            RowsEventData::UpdateRowsEvent(ev) => ev.table_id(),
            RowsEventData::DeleteRowsEvent(ev) => ev.table_id(),
            _ => return Ok(()),
        };

        let tm_guard = self.table_map_events.read().unwrap();
        let tm = tm_guard.get(&table_id).context("Missing TableMapEvent")?;
        let table_name = tm.table_name().to_string();

        let table_lock = self.table_manager.get_table(&self.entity, &table_name)?;
        let mut table = table_lock.write().unwrap();

        let pk_field = state.pk_map.get(&table_name).cloned().unwrap_or_else(|| "PK".to_string());
        table.manifest.primary_key = pk_field.clone();

        match rows_data {
            RowsEventData::WriteRowsEvent(ev) => {
                for row_pair in ev.rows(tm) {
                    if let Ok((Some(binlog_row), _)) = row_pair {
                        let row = Row::try_from(binlog_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
                        let data = self.parse_row(row, &table_name)?;
                        table.insert(data)?;
                    }
                }
            }
            RowsEventData::UpdateRowsEvent(ev) => {
                for row_pair in ev.rows(tm) {
                    if let Ok((_, Some(after_row))) = row_pair {
                        let row = Row::try_from(after_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
                        let data = self.parse_row(row, &table_name)?;
                        if let Some(pk_val) = data.get(&pk_field).cloned() {
                            table.update(&pk_val, data)?;
                        }
                    }
                }
            }
            RowsEventData::DeleteRowsEvent(ev) => {
                for row_pair in ev.rows(tm) {
                    if let Ok((Some(binlog_row), _)) = row_pair {
                        let row = Row::try_from(binlog_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
                        let data = self.parse_row(row, &table_name)?;
                        if let Some(pk_val) = data.get(&pk_field) {
                            table.delete(pk_val)?;
                        }
                    }
                }
            }
            _ => {}
        }
        table.flush_active_segment()?;
        Ok(())
    }

    fn parse_row(&self, row: Row, table_name: &str) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();
        let maps = self.column_maps.read().unwrap();
        let columns_names = maps.get(table_name).context("Column map not found for table")?;
        
        for i in 0..row.len() {
            let col_name = columns_names.get(i).cloned().unwrap_or_else(|| format!("col_{}", i));
            
            let mut val_str = match row.get_opt::<mysql_common::Value, usize>(i) {
                Some(Ok(v)) => match v {
                    mysql_common::Value::NULL => "".to_string(),
                    mysql_common::Value::Bytes(ref b) => String::from_utf8_lossy(b).to_string(),
                    mysql_common::Value::Date(y, m, d, h, min, s, ms) => format!("{}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}", y, m, d, h, min, s, ms),
                    _ => format!("{:?}", v),
                },
                _ => "".to_string(),
            };

            if val_str.starts_with("Int(") || val_str.starts_with("UInt(") {
                if let Some(start) = val_str.find('(') {
                    if let Some(end) = val_str.find(')') {
                        val_str = val_str[start+1..end].to_string();
                    }
                }
            }

            map.insert(col_name, val_str);
        }
        Ok(map)
    }
}
