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
use crate::core::date_utils::{extract_day, extract_month, extract_hour_bucket, is_date_format, has_time_component};
use tracing::{info, debug, warn, error};

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
    /// Folder under `data/` for `cdc_config.json` / `cdc_state.json` (connection profile).
    entity: String,
    database: String,
    /// When true: one binlog stream for the server; data under `data/<schema>/` using real DB names.
    sync_all_databases: bool,
    state_path: String,
    table_manager: Arc<TableManager>,
    column_maps: Arc<RwLock<HashMap<String, Vec<String>>>>,
    date_columns: Arc<RwLock<HashMap<String, Vec<String>>>>, // table -> list of date column names
    enum_maps: Arc<RwLock<HashMap<String, HashMap<String, Vec<String>>>>>, // table -> column -> values
    table_map_events: Arc<RwLock<HashMap<u64, TableMapEvent<'static>>>>,
    log_tx: Option<tokio::sync::mpsc::Sender<String>>,
}

impl CdcWorker {
    pub fn new(url: String, entity: String, database: String) -> Self {
        Self::with_log(url, entity, database, None)
    }

    /// Full-server sync: one binlog stream; tables stored under `data/<mysql_schema>/`.
    pub fn new_sync_all(url: String, entity: String) -> Self {
        Self::with_manager_and_log(
            url,
            entity,
            String::new(),
            Arc::new(TableManager::new()),
            None,
            true,
        )
    }

    pub fn with_log(url: String, entity: String, database: String, log_tx: Option<tokio::sync::mpsc::Sender<String>>) -> Self {
        Self::with_manager_and_log(
            url,
            entity,
            database,
            Arc::new(TableManager::new()),
            log_tx,
            false,
        )
    }

    pub fn with_manager(url: String, entity: String, database: String, table_manager: Arc<TableManager>) -> Self {
        Self::with_manager_and_log(url, entity, database, table_manager, None, false)
    }

    pub fn with_manager_and_log(
        url: String,
        entity: String,
        database: String,
        table_manager: Arc<TableManager>,
        log_tx: Option<tokio::sync::mpsc::Sender<String>>,
        sync_all_databases: bool,
    ) -> Self {
        let state_path = format!("data/{}/cdc_state.json", entity);
        Self {
            url,
            entity,
            database,
            sync_all_databases,
            state_path,
            table_manager,
            column_maps: Arc::new(RwLock::new(HashMap::new())),
            date_columns: Arc::new(RwLock::new(HashMap::new())),
            enum_maps: Arc::new(RwLock::new(HashMap::new())),
            table_map_events: Arc::new(RwLock::new(HashMap::new())),
            log_tx,
        }
    }

    fn mysql_ident(ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn mysql_string_literal(s: &str) -> String {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
    }

    /// Fully-qualified `schema`.`table` for SQL (uses server schema casing).
    fn qualified_schema_table(schema: &str, table: &str) -> String {
        format!(
            "{}.{}",
            Self::mysql_ident(schema),
            Self::mysql_ident(table)
        )
    }

    fn is_system_schema(name: &str) -> bool {
        matches!(
            name.to_lowercase().as_str(),
            "information_schema"
                | "mysql"
                | "performance_schema"
                | "sys"
                | "ndbinfo"
                | "mysql_innodb_cluster_metadata"
        )
    }

    /// Row map key: `schema.table` in sync-all mode (schema lowercased for stable binlog match),
    /// plain `table` in single-DB mode.
    fn qualified_table_key(sync_all: bool, schema: &str, table: &str) -> String {
        if sync_all {
            format!("{}.{}", schema.to_lowercase(), table)
        } else {
            table.to_string()
        }
    }

    async fn list_user_schemas(conn: &mut Conn) -> Result<Vec<String>> {
        // Use explicit row type: `Vec<String>` with SHOW DATABASES can deserialize incorrectly
        // on some setups (e.g. a single concatenated value), breaking multi-schema sync.
        let rows: Vec<(String,)> = conn
            .query("SHOW DATABASES")
            .await
            .context("SHOW DATABASES")?;
        Ok(rows
            .into_iter()
            .map(|(s,)| s)
            .filter(|s| !Self::is_system_schema(s))
            .collect())
    }

    fn log_info(&self, msg: String) {
        if let Some(tx) = &self.log_tx {
            let _ = tx.try_send(msg.clone());
        } else {
            info!("{}", msg);
        }
    }

    fn log_error(&self, msg: String) {
        if let Some(tx) = &self.log_tx {
            let _ = tx.try_send(format!("CDC_ERROR: {}", msg));
        } else {
            error!("{}", msg);
        }
    }

    fn log_warn(&self, msg: String) {
        if let Some(tx) = &self.log_tx {
            let _ = tx.try_send(format!("WARN: {}", msg));
        } else {
            warn!("{}", msg);
        }
    }

    async fn enter_static_mode(&self, reason: String) -> Result<()> {
        self.log_warn(reason);
        self.log_info("CDC: Real-time sync inactive. Operating with static data only.".to_string());
        if let Some(tx) = &self.log_tx {
            let _ = tx.try_send("CDC_DISABLED".to_string());
        }
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
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

    /// MySQL replication error 1236 and variants: saved file/pos no longer on server (purged, new volume, etc.).
    fn is_stale_saved_binlog_error(msg: &str) -> bool {
        let m = msg.to_lowercase();
        m.contains("(1236)")
            || m.contains(" 1236:")
            || m.contains("error 1236")
            || m.contains("could not open log file")
            || m.contains("could not find first log file")
    }

    async fn resolve_binlog_position(&self, conn: &mut Conn, state: &mut CdcState) -> Result<()> {
        if !state.binlog_file.is_empty() {
            return Ok(());
        }

        // Try modern MySQL command first (MySQL 8.4+)
        let mut last_err = String::new();
        let mut row: Option<Row> = match conn.query_first("SHOW BINARY LOG STATUS").await {
            Ok(r) => r,
            Err(e) => {
                last_err = e.to_string();
                None
            }
        };

        // Fallback to older MySQL command (MySQL < 8.4)
        if row.is_none() {
            row = match conn.query_first("SHOW MASTER STATUS").await {
                Ok(r) => r,
                Err(e) => {
                    let e_msg = e.to_string();
                    // If this also fails, we decide what to log. 
                    // If the first error was a permission issue (not syntax), it's more important.
                    if !last_err.is_empty() && !last_err.contains("1064") {
                        debug!("CDC Binlog check: {}", last_err);
                    } else if !e_msg.contains("1064") {
                        debug!("CDC Binlog check: {}", e_msg);
                    } else {
                        debug!("CDC Binlog check: Command not supported or access denied.");
                    }
                    None
                }
            };
        }

        if let Some(r) = row {
            // Extract file (column 0) and position (column 1)
            let file: String = r.get(0).unwrap_or_default();
            let pos: u32 = r.get(1).unwrap_or(4);
            
            if !file.is_empty() {
                state.binlog_file = file;
                state.binlog_pos = pos;
                self.log_info(format!("CDC: Real-time sync enabled. Starting from {} at position {}", state.binlog_file, state.binlog_pos));
            }
        } else {
            self.log_info("CDC: Operating in Static Data mode (Real-time updates not enabled on MySQL).".to_string());
        }
        Ok(())
    }

    async fn fetch_all_tables(&self, conn: &mut Conn) -> Result<Vec<String>> {
        // `Vec<String>` + SHOW TABLES is unreliable across mysql_async / server combos.
        // information_schema + one column per row avoids a single bogus "table name".
        let rows: Vec<(String,)> = conn
            .query(
                "SELECT TABLE_NAME FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' \
                 ORDER BY TABLE_NAME",
            )
            .await
            .context("list tables (information_schema)")?;
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    /// List base tables for a schema without relying on `USE` / `DATABASE()`.
    async fn fetch_all_tables_in_schema(conn: &mut Conn, schema: &str) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT TABLE_NAME FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {} AND TABLE_TYPE = 'BASE TABLE' \
             ORDER BY TABLE_NAME",
            Self::mysql_string_literal(schema)
        );
        let rows: Vec<(String,)> = conn
            .query(sql)
            .await
            .context("list tables (information_schema, named schema)")?;
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    async fn fetch_column_info(
        &self,
        conn: &mut Conn,
        schema: &str,
        qualified_table_key: &str,
        table_name: &str,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let fq = Self::qualified_schema_table(schema, table_name);
        let rows: Vec<(String, String, String, String, Option<String>, String)> =
            conn.query(format!("DESCRIBE {}", fq)).await?;

        let mut all_cols = Vec::new();
        let mut date_cols = Vec::new();
        let mut enum_info = HashMap::new();

        for row in rows {
            let col_name = row.0;
            let col_type_raw = row.1;
            let col_type_lower = col_type_raw.to_lowercase();
            all_cols.push(col_name.clone());

            if col_type_lower.contains("date") || col_type_lower.contains("timestamp") {
                date_cols.push(col_name.clone());
            }

            // Detect ENUM and extract values (Preserving Case)
            if col_type_lower.starts_with("enum(") {
                let values_str = col_type_raw
                    .trim_start_matches(|c| c != '(')
                    .trim_start_matches('(')
                    .trim_end_matches(')');
                let values: Vec<String> = values_str
                    .split(',')
                    .map(|v| v.trim_matches('\'').to_string())
                    .collect();
                enum_info.insert(col_name, values);
            }
        }

        if !enum_info.is_empty() {
            let mut maps = self.enum_maps.write().unwrap();
            maps.insert(qualified_table_key.to_string(), enum_info);
        }

        Ok((all_cols, date_cols))
    }

    async fn bootstrap_table(
        &self,
        conn: &mut Conn,
        schema: &str,
        table_name: &str,
        state: &mut CdcState,
    ) -> Result<()> {
        let qkey = Self::qualified_table_key(self.sync_all_databases, schema, table_name);
        let disk_entity: String = if self.sync_all_databases {
            schema.to_lowercase()
        } else {
            self.entity.clone()
        };

        self.log_info(format!(
            "CDC: Bootstrapping table '{}' (schema '{}')...",
            table_name, schema
        ));

        let (cols, dates) = self
            .fetch_column_info(conn, schema, &qkey, table_name)
            .await?;
        {
            let mut maps = self.column_maps.write().unwrap();
            maps.insert(qkey.clone(), cols.clone());
            let mut d_maps = self.date_columns.write().unwrap();
            d_maps.insert(qkey.clone(), dates.clone());
        }

        let table_lock = self.table_manager.get_table(&disk_entity, table_name)?;
        let mut table = table_lock.write().unwrap();

        // Save the original fields in the manifest
        let _ = table.set_original_fields(cols.clone());

        let pk_query = format!(
            "SELECT COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} \
             AND INDEX_NAME = 'PRIMARY' ORDER BY SEQ_IN_INDEX LIMIT 1",
            Self::mysql_string_literal(schema),
            Self::mysql_string_literal(table_name)
        );
        let pk_col: Option<String> = conn.query_first(pk_query).await?;

        if let Some(col) = pk_col {
            table.manifest.primary_key = col.clone();
            state.pk_map.insert(qkey.clone(), col);
            debug!(
                "CDC: Detected PK='{}' for table '{}'",
                table.manifest.primary_key, qkey
            );
        } else if let Some(pk_cand) = cols.iter().find(|c| c.ends_with("_id") || *c == "id") {
            table.manifest.primary_key = pk_cand.clone();
            state.pk_map.insert(qkey.clone(), pk_cand.clone());
        }

        let fq = Self::qualified_schema_table(schema, table_name);
        let mut result_set = conn
            .query_iter(format!("SELECT * FROM {}", fq))
            .await?;
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
                
                // Date expansion if applicable
                if dates.contains(col_name) && is_date_format(&val_str) {
                    if let Some(d) = extract_day(&val_str) { data.insert(format!("{}_day", col_name), d); }
                    if let Some(m) = extract_month(&val_str) { data.insert(format!("{}_month", col_name), m); }
                    if has_time_component(&val_str) {
                        if let Some(h) = extract_hour_bucket(&val_str) { data.insert(format!("{}_hour_bucket", col_name), h); }
                    }
                }

                data.insert(col_name.clone(), val_str);
            }
            table.insert(data)?;
            count += 1;
        }

        table.flush_active_segment()?;
        self.log_info(format!(
            "CDC: Table '{}' synchronized ({} rows).",
            qkey, count
        ));
        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        if self.sync_all_databases {
            self.log_info("CDC: Connecting to MySQL (sync all databases on server)...".to_string());
        } else {
            self.log_info(format!("CDC: Connecting to MySQL on DB '{}'...", self.database));
        }
        let mut final_url = self.url.clone();
        
        // Host translation for Docker (macOS/Windows)
        let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
        if is_docker {
            if final_url.contains("@localhost") {
                final_url = final_url.replace("@localhost", "@host.docker.internal");
            } else if final_url.contains("@127.0.0.1") {
                final_url = final_url.replace("@127.0.0.1", "@host.docker.internal");
            }
        }

        let opts = match Opts::from_url(&final_url) {
            Ok(o) => o,
            Err(e) => {
                self.log_error(format!("Invalid URL: {}", e));
                return Err(e.into());
            }
        };
        let pool = Pool::new(opts);
        self.log_info(format!("CDC: Connecting to MySQL at {}...", final_url.split('@').last().unwrap_or("unknown")));
        
        let mut conn = match tokio::time::timeout(std::time::Duration::from_secs(30), pool.get_conn()).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                self.log_error(format!("CDC: Connection failed: {}", e));
                return Err(e.into());
            }
            Err(_) => {
                self.log_error("CDC: Connection timed out after 30 seconds. Check network/firewall.".to_string());
                return Err(anyhow::anyhow!("Connection timeout"));
            }
        };

        self.log_info("CDC: Successfully connected. Checking Binlog status...".to_string());

        if self.sync_all_databases {
            if let Err(e) = conn.query_drop("USE information_schema").await {
                self.log_error(format!("CDC: USE information_schema failed: {}", e));
                return Err(e.into());
            }
        } else if let Err(e) = conn
            .query_drop(format!("USE {}", Self::mysql_ident(&self.database)))
            .await
        {
            self.log_error(format!("Database '{}' not found: {}", self.database, e));
            return Err(e.into());
        }

        let mut state = self.load_state();
        if self.sync_all_databases {
            let all_simple = !state.bootstrapped_tables.is_empty()
                && state.bootstrapped_tables.iter().all(|k| !k.contains('.'));
            if all_simple {
                self.log_warn(
                    "CDC: Existing cdc_state used single-table keys; resetting bootstrap metadata for sync-all."
                        .to_string(),
                );
                state.bootstrapped_tables.clear();
                state.pk_map.clear();
                let _ = self.save_state(&state);
            }
        }

        if self.sync_all_databases {
            let schemas = match Self::list_user_schemas(&mut conn).await {
                Ok(s) => s,
                Err(e) => {
                    self.log_error(format!("Failed to list databases: {}", e));
                    return Err(e.into());
                }
            };
            self.log_info(format!(
                "CDC: Discovered {} user database(s) to sync.",
                schemas.len()
            ));
            for schema in schemas {
                let tables = match Self::fetch_all_tables_in_schema(&mut conn, &schema).await {
                    Ok(t) => t,
                    Err(e) => {
                        self.log_warn(format!(
                            "CDC: Skipping database '{}' (listing tables failed): {}",
                            schema, e
                        ));
                        continue;
                    }
                };
                self.log_info(format!(
                    "CDC: Schema '{}' — {} base table(s) to process.",
                    schema,
                    tables.len()
                ));
                for table_name in &tables {
                    let qkey = Self::qualified_table_key(true, &schema, table_name);
                    if !state.bootstrapped_tables.contains(&qkey) {
                        if let Err(e) = self
                            .bootstrap_table(&mut conn, &schema, table_name, &mut state)
                            .await
                        {
                            return self
                                .enter_static_mode(format!(
                                    "Bootstrap failed for '{}': {}. Falling back to static data mode.",
                                    qkey, e
                                ))
                                .await;
                        }
                        state.bootstrapped_tables.push(qkey.clone());
                        self.save_state(&state)?;
                    } else {
                        let (cols, dates) = match self
                            .fetch_column_info(&mut conn, &schema, &qkey, table_name)
                            .await
                        {
                            Ok(info) => info,
                            Err(e) => {
                                return self
                                    .enter_static_mode(format!(
                                        "Failed to refresh schema for '{}': {}. Falling back to static data mode.",
                                        qkey, e
                                    ))
                                    .await;
                            }
                        };
                        {
                            let mut maps = self.column_maps.write().unwrap();
                            maps.insert(qkey.clone(), cols);
                            let mut d_maps = self.date_columns.write().unwrap();
                            d_maps.insert(qkey.clone(), dates);
                        }
                    }
                }
            }
        } else {
            let tables = match self.fetch_all_tables(&mut conn).await {
                Ok(t) => t,
                Err(e) => {
                    self.log_error(format!("Failed to fetch tables: {}", e));
                    return Err(e.into());
                }
            };
            self.log_info(format!(
                "CDC: Database '{}' — {} base table(s) to process.",
                self.database,
                tables.len()
            ));

            for table_name in &tables {
                let qkey = Self::qualified_table_key(false, &self.database, table_name);
                if !state.bootstrapped_tables.contains(&qkey) {
                    if let Err(e) = self
                        .bootstrap_table(&mut conn, self.database.as_str(), table_name, &mut state)
                        .await
                    {
                        return self
                            .enter_static_mode(format!(
                                "Bootstrap failed for '{}': {}. Falling back to static data mode.",
                                table_name, e
                            ))
                            .await;
                    }
                    state.bootstrapped_tables.push(qkey.clone());
                    self.save_state(&state)?;
                } else {
                    let (cols, dates) = match self
                        .fetch_column_info(
                            &mut conn,
                            self.database.as_str(),
                            &qkey,
                            table_name,
                        )
                        .await
                    {
                        Ok(info) => info,
                        Err(e) => {
                            return self
                                .enter_static_mode(format!(
                                    "Failed to refresh schema for '{}': {}. Falling back to static data mode.",
                                    table_name, e
                                ))
                                .await;
                        }
                    };
                    {
                        let mut maps = self.column_maps.write().unwrap();
                        maps.insert(qkey.clone(), cols);
                        let mut d_maps = self.date_columns.write().unwrap();
                        d_maps.insert(qkey.clone(), dates);
                    }
                }
            }
        }

        self.resolve_binlog_position(&mut conn, &mut state).await?;

        if state.binlog_file.is_empty() {
            return self.enter_static_mode("CDC is not enabled on server.".to_string()).await;
        }

        // Notify UI that bootstrap is complete and we are entering live mode
        if let Some(tx) = &self.log_tx {
            let _ = tx.try_send("CDC_READY".to_string());
        }

        let pool = pool.clone();
        let mut last_flush = std::time::Instant::now();

        'binlog_retry: loop {
            self.log_info(format!("CDC: Resuming live stream from {}:{}", state.binlog_file, state.binlog_pos));

            let server_id = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() % 100000) as u32 + 1000;

            let request = BinlogStreamRequest::new(server_id)
                .with_filename(state.binlog_file.as_bytes())
                .with_pos(state.binlog_pos as u64);

            let mut stream = match conn.get_binlog_stream(request).await {
                Ok(s) => s,
                Err(e) => {
                    let msg = e.to_string();
                    if Self::is_stale_saved_binlog_error(&msg) {
                        warn!("CDC: Saved binlog no longer available on server ({}). Re-syncing from current head.", msg);
                        state.binlog_file.clear();
                        state.binlog_pos = 4;
                        self.save_state(&state)?;
                        conn = pool.get_conn().await?;
                        if self.sync_all_databases {
                            conn.query_drop("USE information_schema").await?;
                        } else {
                            conn.query_drop(format!("USE {}", Self::mysql_ident(&self.database)))
                                .await?;
                        }
                        continue 'binlog_retry;
                    }
                    return self.enter_static_mode(format!(
                        "Binlog stream unavailable: {}. Falling back to static data mode.",
                        msg
                    )).await;
                }
            };

            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(e) => e,
                    Err(e) => {
                        let msg = e.to_string();
                        if Self::is_stale_saved_binlog_error(&msg) {
                            warn!("CDC: Binlog stream error ({}). Re-syncing from current head.", msg);
                            state.binlog_file.clear();
                            state.binlog_pos = 4;
                            self.save_state(&state)?;
                            conn = pool.get_conn().await?;
                            if self.sync_all_databases {
                                conn.query_drop("USE information_schema").await?;
                            } else {
                                conn.query_drop(format!("USE {}", Self::mysql_ident(&self.database)))
                                    .await?;
                            }
                            continue 'binlog_retry;
                        }
                        return self.enter_static_mode(format!(
                            "Binlog stream error: {}. Falling back to static data mode.",
                            msg
                        )).await;
                    }
                };
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

            return Ok(());
        }
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
        let schema = tm.database_name().to_string();
        let table_name = tm.table_name().to_string();

        if self.sync_all_databases && Self::is_system_schema(&schema) {
            return Ok(());
        }

        let qkey = Self::qualified_table_key(self.sync_all_databases, &schema, &table_name);
        let disk_entity = if self.sync_all_databases {
            schema.to_lowercase()
        } else {
            self.entity.clone()
        };

        {
            let maps = self.column_maps.read().unwrap();
            if !maps.contains_key(&qkey) {
                debug!(
                    "CDC: Ignoring row event for '{}' (not bootstrapped / unknown)",
                    qkey
                );
                return Ok(());
            }
        }

        let table_lock = self.table_manager.get_table(&disk_entity, &table_name)?;
        let mut table = table_lock.write().unwrap();

        let pk_field = state
            .pk_map
            .get(&qkey)
            .cloned()
            .unwrap_or_else(|| "PK".to_string());
        table.manifest.primary_key = pk_field.clone();

        match rows_data {
            RowsEventData::WriteRowsEvent(ev) => {
                debug!("CDC: Received Write event for table '{}'", qkey);
                for row_pair in ev.rows(tm) {
                    if let Ok((Some(binlog_row), _)) = row_pair {
                        let row = Row::try_from(binlog_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
                        let data = self.parse_row(row, &qkey)?;
                        let pk_val = data.get(&pk_field).cloned().unwrap_or_default();
                        table.insert(data.clone())?;
                        
                        // Emit broadcast event
                        let _ = self.table_manager.events_tx.send(crate::server::table_manager::TableUpdateEvent {
                            entity: disk_entity.clone(),
                            table_name: table_name.clone(),
                            event_type: "INSERT".to_string(),
                            pk: pk_val,
                            row: table.manifest.original_fields.iter().map(|f| data.get(f).cloned().unwrap_or_default()).collect(),
                        });
                    }
                }
            }
            RowsEventData::UpdateRowsEvent(ev) => {
                for row_pair in ev.rows(tm) {
                    if let Ok((_, Some(after_row))) = row_pair {
                        let row = Row::try_from(after_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
                        let data = self.parse_row(row, &qkey)?;
                        if let Some(pk_val) = data.get(&pk_field).cloned() {
                            table.update(&pk_val, data.clone())?;
                            
                            // Emit broadcast event
                            let _ = self.table_manager.events_tx.send(crate::server::table_manager::TableUpdateEvent {
                                entity: disk_entity.clone(),
                                table_name: table_name.clone(),
                                event_type: "UPDATE".to_string(),
                                pk: pk_val,
                                row: table.manifest.original_fields.iter().map(|f| data.get(f).cloned().unwrap_or_default()).collect(),
                            });
                        }
                    }
                }
            }
            RowsEventData::DeleteRowsEvent(ev) => {
                for row_pair in ev.rows(tm) {
                    if let Ok((Some(binlog_row), _)) = row_pair {
                        let row = Row::try_from(binlog_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
                        let data = self.parse_row(row, &qkey)?;
                        if let Some(pk_val) = data.get(&pk_field) {
                            let pk_copy = pk_val.clone();
                            table.delete(pk_val)?;
                            
                            // Emit broadcast event
                            let _ = self.table_manager.events_tx.send(crate::server::table_manager::TableUpdateEvent {
                                entity: disk_entity.clone(),
                                table_name: table_name.clone(),
                                event_type: "DELETE".to_string(),
                                pk: pk_copy,
                                row: vec![],
                            });
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
        
        let d_maps = self.date_columns.read().unwrap();
        let dates = d_maps.get(table_name).cloned().unwrap_or_default();

        let e_maps = self.enum_maps.read().unwrap();
        let table_enums = e_maps.get(table_name);

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

            // Dynamic ENUM translation (MySQL Binlog sends numeric indices)
            if let Some(enums) = table_enums {
                if let Some(values) = enums.get(&col_name) {
                    if !val_str.is_empty() && val_str.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(idx) = val_str.parse::<usize>() {
                            if idx > 0 && idx <= values.len() {
                                val_str = values[idx - 1].clone();
                            }
                        }
                    }
                }
            }

            // If it is a known date column but comes as a number (TIMESTAMP in binlog),
            // convert it to a readable format so that the date expansion works.
            if dates.contains(&col_name) && !val_str.is_empty() && val_str.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(timestamp) = val_str.parse::<i64>() {
                    // Only convert if it seems like a timestamp ( > year 2000 approx )
                    if timestamp > 946684800 {
                        use chrono::{TimeZone, Utc};
                        let dt = Utc.timestamp_opt(timestamp, 0).single();
                        if let Some(dt) = dt {
                            val_str = dt.format("%Y-%m-%d %H:%M:%S").to_string();
                        }
                    }
                }
            }

            // Date expansion
            if dates.contains(&col_name) && is_date_format(&val_str) {
                if let Some(d) = extract_day(&val_str) { map.insert(format!("{}_day", col_name), d); }
                if let Some(m) = extract_month(&val_str) { map.insert(format!("{}_month", col_name), m); }
                if has_time_component(&val_str) {
                    if let Some(h) = extract_hour_bucket(&val_str) { map.insert(format!("{}_hour_bucket", col_name), h); }
                }
            }

            map.insert(col_name, val_str);
        }
        Ok(map)
    }
}
