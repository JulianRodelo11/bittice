use anyhow::{Context, Result};
use mysql_async::prelude::*;
use mysql_async::{BinlogStream, Conn, Opts, OptsBuilder, Pool, BinlogStreamRequest};
use mysql_common::packets::Sid;
use mysql_common::binlog::events::{EventData, RowsEventData, RowsEventRows, TableMapEvent};
use mysql_common::packets::Column;
use mysql_common::row::Row;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio_stream::StreamExt;
use uuid::Uuid;
use crate::core::storage::table::Table;
use crate::server::table_manager::{TableManager, TableUpdateEvent};
use crate::core::date_utils::{extract_day, extract_month, extract_hour_bucket, is_date_format, has_time_component};
use tracing::{info, debug, warn, error};

/// Result of the staged CDC pipeline used to gate HTTP until each profile finishes Phase 4 (or static/fail).
#[derive(Debug, Clone)]
pub struct CdcStartupReport {
    pub entity: String,
    pub outcome: CdcStartupOutcome,
}

#[derive(Debug, Clone)]
pub enum CdcStartupOutcome {
    LiveReplication,
    StaticMirror,
    Failed(String),
}

/// Upper bound on replay lag after crash during CDC (wall clock).
const CDC_STATE_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Cap burst writes when many small binlog events arrive within [`CDC_STATE_SAVE_INTERVAL`].
const CDC_STATE_SAVE_EVENT_BURST: u32 = 1024;
/// Wake `binlog` `stream.next()` waits periodically so [`crate::server::engine_halt_requested`] is honored even when the server is quiet.
const CDC_ENGINE_HALT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

fn cdc_health_check_interval() -> std::time::Duration {
    std::time::Duration::from_secs(
        std::env::var("BITTICE_CDC_HEALTH_CHECK_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
    )
}

/// Guard for the one-line startup hint when `BITTICE_CDC_LOG_ROW_EVENTS` is enabled.
static CDC_ROW_TRACE_BANNER_EMITTED: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CdcState {
    pub binlog_file: String,
    pub binlog_pos: u32,
    pub bootstrapped_tables: Vec<String>,
    #[serde(default)]
    pub pk_map: HashMap<String, String>,
    /// Rolling GTID set aligned with processed transactions (bootstrap snapshot plus merges from binlog `GtidEvent`s).
    #[serde(default)]
    pub gtid_executed: String,
    /// When true, MVCC bootstrap still needs binlog catch-up before the mirror matches MySQL at snapshot end.
    #[serde(default)]
    pub mvcc_snapshot_catchup_pending: bool,
    /// Last values seen from MySQL at CDC startup (persisted in `cdc_state.json`).
    #[serde(default)]
    pub observed_binlog_format_global: Option<String>,
    #[serde(default)]
    pub observed_binlog_format_session: Option<String>,
    /// Updated whenever a row batch is successfully applied to the mirror (`applied > 0`).
    #[serde(default)]
    pub last_mirror_batch_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_mirror_batch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapMode {
    /// `SELECT *` snapshot then CDC (initial menu sync).
    FullSnapshot,
    /// DESCRIBE + empty mirror during binlog replay; avoids duplicate rows vs replayed INSERTs.
    SchemaOnly,
}

pub struct CdcWorker {
    url: String,
    /// Folder name under `data/profiles/<entity>/` for `cdc_config.json` / `cdc_state.json`.
    entity: String,
    database: String,
    /// When true: one binlog stream for the server; data under `data/mirror/<schema>/` using real DB names.
    sync_all_databases: bool,
    state_path: String,
    table_manager: Arc<TableManager>,
    column_maps: Arc<RwLock<HashMap<String, Vec<String>>>>,
    date_columns: Arc<RwLock<HashMap<String, Vec<String>>>>, // table -> list of date column names
    enum_maps: Arc<RwLock<HashMap<String, HashMap<String, Vec<String>>>>>, // table -> column -> values
    table_map_events: Arc<RwLock<HashMap<u64, TableMapEvent<'static>>>>,
    log_tx: Option<tokio::sync::mpsc::Sender<String>>,
    /// When true, after bootstrap open the binlog, apply the snapshot backlog until the mirror reaches the
    /// master tip (idle poll + coordinate check), emit `CDC_READY`, then return. The connect wizard uses this
    /// so the CLI does not hold a permanent binlog connection; the server should run continuous CDC next.
    exit_after_cdc_ready: bool,
    /// When set, signals once when Phase 4 completes (live), static fallback activates, or the worker fatally errors.
    startup_report_tx: Option<std::sync::mpsc::Sender<CdcStartupReport>>,
    startup_report_sent: AtomicBool,
    /// Per-mirror-table key `entity/table_dir`: row batches applied since last `flush_active_segment`.
    cdc_mirror_flush_ticks: Arc<RwLock<HashMap<String, u32>>>,
    /// When set (from `sync_tables` in `cdc_config.json`), only these tables participate in snapshot + binlog mirror.
    sync_table_allowlist: Arc<RwLock<Option<HashSet<String>>>>,
}

impl CdcWorker {
    pub fn new(url: String, entity: String, database: String) -> Self {
        Self::with_log(url, entity, database, None)
    }

    /// Full-server sync: one binlog stream; tables stored under `data/mirror/<mysql_schema>/`.
    pub fn new_sync_all(url: String, entity: String) -> Self {
        Self::with_manager_and_log(
            url,
            entity,
            String::new(),
            Arc::new(TableManager::new()),
            None,
            true,
            false,
            None,
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
            false,
            None,
        )
    }

    pub fn with_manager(url: String, entity: String, database: String, table_manager: Arc<TableManager>) -> Self {
        Self::with_manager_and_log(url, entity, database, table_manager, None, false, false, None)
    }

    pub fn with_manager_and_log(
        url: String,
        entity: String,
        database: String,
        table_manager: Arc<TableManager>,
        log_tx: Option<tokio::sync::mpsc::Sender<String>>,
        sync_all_databases: bool,
        exit_after_cdc_ready: bool,
        startup_report_tx: Option<std::sync::mpsc::Sender<CdcStartupReport>>,
    ) -> Self {
        let state_path = crate::core::data_paths::profile_dir(&entity)
            .join("cdc_state.json")
            .to_string_lossy()
            .into_owned();
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
            exit_after_cdc_ready,
            startup_report_tx,
            startup_report_sent: AtomicBool::new(false),
            cdc_mirror_flush_ticks: Arc::new(RwLock::new(HashMap::new())),
            sync_table_allowlist: Arc::new(RwLock::new(None)),
        }
    }

    /// Inspect user schemas and base tables (connect wizard). `single_database == None` scans every user DB.
    pub async fn discover_mysql_catalog(
        url: &str,
        single_database: Option<&str>,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let raw_opts = Opts::from_url(url).context("invalid MySQL URL for catalog discovery")?;
        let opts_with_keepalive: Opts = OptsBuilder::from_opts(raw_opts.clone())
            .tcp_keepalive(Some(30u32))
            .into();
        let opts_without_keepalive: Opts = OptsBuilder::from_opts(raw_opts).into();
        let mut pool = Pool::new(opts_with_keepalive);
        let mut conn = match pool.get_conn().await {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("os error 22") || msg.contains("Invalid argument") {
                    pool = Pool::new(opts_without_keepalive);
                    pool.get_conn()
                        .await
                        .context("MySQL catalog discovery: connect after keepalive fallback")?
                } else {
                    return Err(anyhow::Error::new(e)
                        .context("MySQL catalog discovery: connect"));
                }
            }
        };
        match single_database {
            Some(db) => {
                conn.query_drop(format!("USE {}", Self::mysql_ident(db)))
                    .await
                    .with_context(|| format!("USE '{}' for catalog discovery", db))?;
                let tables =
                    Self::fetch_all_tables_in_schema(&mut conn, db).await?;
                Ok(vec![(db.to_string(), tables)])
            }
            None => {
                conn.query_drop("USE information_schema")
                    .await
                    .context("USE information_schema for catalog discovery")?;
                let schemas = Self::list_user_schemas(&mut conn).await?;
                let mut out = Vec::with_capacity(schemas.len());
                for sch in schemas {
                    let tables =
                        Self::fetch_all_tables_in_schema(&mut conn, &sch).await?;
                    out.push((sch, tables));
                }
                Ok(out)
            }
        }
    }

    fn load_sync_table_allowlist_from_config(entity: &str) -> Option<HashSet<String>> {
        let path = crate::core::data_paths::profile_dir(entity).join("cdc_config.json");
        let content = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&content).ok()?;
        let arr = value.get("sync_tables").and_then(|x| x.as_array())?;
        let mut set = HashSet::<String>::new();
        for item in arr {
            let s = item.as_str()?.trim();
            if s.is_empty() {
                continue;
            }
            set.insert(s.to_owned());
        }
        if set.is_empty() {
            None
        } else {
            Some(set)
        }
    }

    /// Keys in `sync_tables` follow [`Self::qualified_table_key`]: lowercased schema + original table when `sync_all`;
    /// otherwise the table name alone.
    fn matches_sync_table_allowlist(
        sync_all: bool,
        schema: &str,
        table: &str,
        allow: &HashSet<String>,
    ) -> bool {
        if sync_all {
            let k1 = format!("{}.{}", schema.to_lowercase(), table);
            if allow.contains(&k1) {
                return true;
            }
            let k2 = format!("{}.{}", schema.to_lowercase(), table.to_lowercase());
            if allow.contains(&k2) {
                return true;
            }
            for entry in allow {
                let Some((es, et)) = entry.split_once('.') else {
                    continue;
                };
                if schema.eq_ignore_ascii_case(es) && et.eq_ignore_ascii_case(table) {
                    return true;
                }
            }
            false
        } else {
            if allow.contains(table) {
                return true;
            }
            if allow.contains(&table.to_lowercase()) {
                return true;
            }
            allow.iter().any(|e| e.eq_ignore_ascii_case(table))
        }
    }

    fn is_table_in_sync_allowlist(&self, schema: &str, table: &str) -> bool {
        let guard = self.sync_table_allowlist.read().unwrap();
        match &*guard {
            None => true,
            Some(set) if set.is_empty() => true,
            Some(set) => Self::matches_sync_table_allowlist(
                self.sync_all_databases,
                schema,
                table,
                set,
            ),
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

    /// With `sync_all_databases = false`, MySQL still ships **one server-wide binlog**; events include
    /// writes to every schema on the instance. We only mirror `self.database` — everything else is skipped.
    #[inline]
    fn is_out_of_scope_binlog_schema(&self, schema: &str) -> bool {
        !self.sync_all_databases && !schema.eq_ignore_ascii_case(&self.database)
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

    /// Align binlog table identifiers with bootstrap keys (`column_maps`, `pk_map` in [`CdcState`]).
    /// RDS often lowercases names in the binlog while `information_schema` keeps mixed case.
    fn resolve_cdc_table_keys(
        sync_all: bool,
        schema: &str,
        binlog_table: &str,
        maps: &HashMap<String, Vec<String>>,
    ) -> Option<String> {
        if sync_all {
            let schema_l = schema.to_lowercase();
            let cand_a = format!("{}.{}", schema_l, binlog_table);
            let cand_b = format!("{}.{}", schema_l, binlog_table.to_lowercase());
            if maps.contains_key(&cand_a) {
                return Some(cand_a);
            }
            if maps.contains_key(&cand_b) {
                return Some(cand_b);
            }
            let bt_lower = binlog_table.to_lowercase();
            for key in maps.keys() {
                let Some((s, t)) = key.split_once('.') else {
                    continue;
                };
                if s.to_lowercase() == schema_l && t.to_lowercase() == bt_lower {
                    return Some(key.clone());
                }
            }
            None
        } else {
            let cand_a = binlog_table.to_string();
            let cand_b = binlog_table.to_lowercase();
            if maps.contains_key(&cand_a) {
                return Some(cand_a);
            }
            if maps.contains_key(&cand_b) {
                return Some(cand_b);
            }
            let bt_lower = binlog_table.to_lowercase();
            for key in maps.keys() {
                if key.to_lowercase() == bt_lower {
                    return Some(key.clone());
                }
            }
            None
        }
    }

    /// Directory name under `data/mirror/<entity>/` matching how bootstrap created the table tree.
    fn resolve_mirror_table_dir(entity: &str, binlog_table: &str) -> String {
        let base = crate::core::data_paths::mirror_entity_dir(entity);
        let direct = base.join(binlog_table);
        if direct.is_dir() {
            return binlog_table.to_string();
        }
        let want = binlog_table.to_lowercase();
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if name.to_lowercase() == want {
                    return name;
                }
            }
        }
        binlog_table.to_string()
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

    fn log_phase(&self, phase: u8, title: &str) {
        const TOTAL: u8 = 4;
        self.log_info(format!(
            "CDC: [{}] Phase {}/{}: {}",
            self.entity, phase, TOTAL, title
        ));
    }

    fn emit_startup_report(&self, outcome: CdcStartupOutcome) {
        if let Some(tx) = &self.startup_report_tx {
            if self
                .startup_report_sent
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = tx.send(CdcStartupReport {
                    entity: self.entity.clone(),
                    outcome,
                });
            }
        }
    }

    /// Any condition that used to fall back to a static mirror now stops the whole engine instead.
    async fn enter_static_mode(&self, reason: String) -> Result<()> {
        self.emit_startup_report(CdcStartupOutcome::Failed(reason.clone()));
        if let Some(tx) = &self.log_tx {
            let _ = tx.try_send("CDC_DISABLED".to_string());
        }
        self.flush_all_mirror_tables();
        crate::server::request_engine_shutdown_from_cdc(&reason);
        Ok(())
    }

    fn flush_all_mirror_tables(&self) {
        self.table_manager.flush_dirty_tables();
    }

    fn fast_exit_flush(&self) {
        let keys: Vec<String> = {
            let mut dirty = self.table_manager.dirty_tables.write().unwrap();
            dirty.drain().collect()
        };
        if keys.is_empty() {
            return;
        }
        let cache = self.table_manager.tables.read().unwrap();
        for key in &keys {
            if let Some(table_arc) = cache.get(key) {
                if let Ok(mut t) = table_arc.write() {
                    let _ = t.flush_wal_only();
                }
            }
        }
    }

    fn configured_vpn_file_for_entity(&self) -> Option<String> {
        let cfg_path = crate::core::data_paths::profile_dir(&self.entity).join("cdc_config.json");
        let content = std::fs::read_to_string(cfg_path).ok()?;
        let json = serde_json::from_str::<serde_json::Value>(&content).ok()?;
        let vpn_file = json.get("vpn_file")?.as_str()?.trim();
        if vpn_file.is_empty() {
            None
        } else {
            Some(vpn_file.to_string())
        }
    }

    fn try_restart_openvpn_for_route_loss(&self, db_host: &str) -> bool {
        if std::path::Path::new("/.dockerenv").exists() {
            return false;
        }
        let Some(vpn_file) = self.configured_vpn_file_for_entity() else {
            return false;
        };
        self.log_warn(format!(
            "CDC: attempting OpenVPN restart for profile '{}' after route loss.",
            self.entity
        ));
        match crate::core::vpn::VpnManager::prepare_ovpn_file(&vpn_file, db_host) {
            Ok(prepared) => match crate::core::vpn::VpnManager::start(&prepared) {
                Ok(()) => {
                    self.log_info("CDC: OpenVPN restarted; waiting for MySQL route recovery.".to_string());
                    true
                }
                Err(e) => {
                    self.log_warn(format!("CDC: OpenVPN restart failed: {}", e));
                    false
                }
            },
            Err(e) => {
                self.log_warn(format!("CDC: OpenVPN config prepare failed: {}", e));
                false
            }
        }
    }

    fn empty_cdc_state() -> CdcState {
        CdcState {
            binlog_file: String::new(),
            binlog_pos: 4,
            bootstrapped_tables: Vec::new(),
            pk_map: HashMap::new(),
            gtid_executed: String::new(),
            mvcc_snapshot_catchup_pending: false,
            observed_binlog_format_global: None,
            observed_binlog_format_session: None,
            last_mirror_batch_unix_ms: None,
            last_mirror_batch: None,
        }
    }

    fn load_state(&self) -> CdcState {
        match std::fs::File::open(&self.state_path) {
            Ok(file) => match serde_json::from_reader::<_, CdcState>(file) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "CDC: Ignoring corrupt or incompatible {} ({}); treating as first-run CDC metadata.",
                        self.state_path, e
                    );
                    Self::empty_cdc_state()
                }
            },
            Err(_) => Self::empty_cdc_state(),
        }
    }

    fn save_state(&self, state: &CdcState) -> Result<()> {
        let path = Path::new(&self.state_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cdc_state mkdir {:?}", parent.display()))?;
        }
        let Some(parent) = path.parent() else {
            anyhow::bail!("cdc_state path {:?} has no parent directory", path.display());
        };
        let Some(file_name) = path.file_name() else {
            anyhow::bail!("cdc_state path {:?} has no file name", path.display());
        };
        // Unique temp name: a fixed `*.json.tmp` races across processes or overlapping saves
        // (another writer can rename our tmp away → ENOENT). Per-save UUID avoids that.
        let tmp_path = parent.join(format!(
            "{}.{}.tmp",
            file_name.to_string_lossy(),
            Uuid::new_v4()
        ));
        {
            let mut file = std::fs::File::create(&tmp_path)
                .with_context(|| format!("cdc_state create temp {:?}", tmp_path.display()))?;
            serde_json::to_writer_pretty(&mut file, state)?;
            let _ = file.sync_all();
        }
        Self::atomic_rename_into_place(&tmp_path, path)?;
        Ok(())
    }

    /// Replace `dst` with `src` atomically where the platform allows it.
    fn atomic_rename_into_place(src: &Path, dst: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            std::fs::rename(src, dst).with_context(|| {
                format!(
                    "cdc_state rename {:?} -> {:?}",
                    src.display(),
                    dst.display()
                )
            })?;
        }
        #[cfg(not(unix))]
        {
            if dst.exists() {
                std::fs::remove_file(dst).with_context(|| {
                    format!("cdc_state remove {:?} before replace", dst.display())
                })?;
            }
            std::fs::rename(src, dst).with_context(|| {
                format!(
                    "cdc_state rename {:?} -> {:?}",
                    src.display(),
                    dst.display()
                )
            })?;
        }
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

    fn env_truthy(var_name: &str) -> bool {
        std::env::var(var_name)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    }

    #[inline]
    fn apply_binlog_file_pos(state: &mut CdcState, next_pos: u32) {
        if next_pos > 0 {
            state.binlog_pos = next_pos;
        }
    }

    /// When the mirror missed an INSERT (bootstrap / binlog gap), apply UPDATE by doing a targeted
    /// `SELECT * … WHERE pk=?` unless `BITTICE_CDC_SKIP_UPDATE_HYDRATE=1`.
    fn cdc_update_hydrate_enabled() -> bool {
        !Self::env_truthy("BITTICE_CDC_SKIP_UPDATE_HYDRATE")
    }

    /// After binlog checkpoint loss (MySQL 1236 / purged logs), rebuild mirrors via `SELECT *` inside an InnoDB
    /// consistent snapshot aligned with captured binlog coordinates — avoids silently skipping offline commits.
    /// Set `BITTICE_CDC_SKIP_MIRROR_RESYNC_ON_GAP=1` to disable (large tables / ops preference).
    fn cdc_mirror_resync_on_binlog_gap_enabled() -> bool {
        !Self::env_truthy("BITTICE_CDC_SKIP_MIRROR_RESYNC_ON_GAP")
    }

    /// Logs each applied binlog row-batch so `data/server.log` + the CLI Live Monitor (`grep CDC`) stay informative.
    /// When `BITTICE_CDC_LOG_ROW_EVENTS` is set, also emits a verbose mysql→mirror line (paths + binlog coords).
    fn maybe_log_mysql_row_batch(
        &self,
        op: &'static str,
        schema: &str,
        mysql_table: &str,
        mirror_entity: &str,
        mirror_table_dir: &str,
        qkey: &str,
        rows: usize,
        state: &CdcState,
    ) {
        if rows == 0 {
            return;
        }
        // Compact line always (matches Live Monitor grep `CDC` without requiring env vars).
        self.log_info(format!(
            "CDC: applied {} {} row(s) on {}.{}",
            op, rows, schema, mysql_table
        ));
        if Self::env_truthy("BITTICE_CDC_LOG_ROW_EVENTS") {
            self.log_info(format!(
                "CDC mysql→mirror {} {}.{} → mirror entity='{}' table_dir='{}' qkey='{}' {} row(s) @ {}:{}",
                op,
                schema,
                mysql_table,
                mirror_entity,
                mirror_table_dir,
                qkey,
                rows,
                state.binlog_file,
                state.binlog_pos,
            ));
        }
    }

    async fn rollback_consistent_snapshot(conn: &mut Conn, active: bool) {
        if active {
            let _ = conn.query_drop("ROLLBACK").await;
        }
    }

    /// Reads `(binlog_file, position)` using `SHOW BINARY LOG STATUS` / `SHOW MASTER STATUS`.
    async fn query_master_coordinates(conn: &mut Conn) -> Result<Option<(String, u32)>> {
        let mut last_err = String::new();
        let mut row: Option<Row> = match conn.query_first("SHOW BINARY LOG STATUS").await {
            Ok(r) => r,
            Err(e) => {
                last_err = e.to_string();
                None
            }
        };

        if row.is_none() {
            row = match conn.query_first("SHOW MASTER STATUS").await {
                Ok(r) => r,
                Err(e) => {
                    let e_msg = e.to_string();
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
            let file: String = r.get(0).unwrap_or_default();
            let pos: u32 = r.get(1).unwrap_or(4);
            if !file.is_empty() {
                return Ok(Some((file, pos)));
            }
        }
        Ok(None)
    }

    /// `SHOW BINARY LOG STATUS` using a pool connection scoped like the CDC worker (schema selection).
    async fn query_master_coordinates_pool(&self, pool: &Pool) -> Result<Option<(String, u32)>> {
        let mut c = pool.get_conn().await?;
        if self.sync_all_databases {
            c.query_drop("USE information_schema").await?;
        } else {
            c.query_drop(format!("USE {}", Self::mysql_ident(&self.database)))
                .await?;
        }
        Self::query_master_coordinates(&mut c).await
    }

    /// Whether persisted replication coordinates are at or past `(tip_file, tip_pos)` from `SHOW BINARY LOG STATUS`.
    fn cdc_replication_caught_up_to_tip(state: &CdcState, tip_file: &str, tip_pos: u32) -> bool {
        if state.binlog_file.is_empty() || tip_file.is_empty() {
            return false;
        }
        match state.binlog_file.as_str().cmp(tip_file) {
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => state.binlog_pos >= tip_pos,
        }
    }

    /// Full current row from MySQL (same shape as bootstrap `SELECT *`) for self-healing CDC gaps.
    async fn fetch_full_row_mysql(
        &self,
        pool: &Pool,
        schema: &str,
        mysql_table: &str,
        qkey: &str,
        pk_field: &str,
        pk_val: &str,
    ) -> Result<Option<HashMap<String, String>>> {
        let mut conn = pool.get_conn().await?;
        if self.sync_all_databases {
            conn.query_drop("USE information_schema").await?;
        } else {
            conn.query_drop(format!("USE {}", Self::mysql_ident(&self.database)))
                .await?;
        }
        let schema_mysql = if self.sync_all_databases {
            Self::resolve_mysql_schema_folder_name(&mut conn, schema).await?
        } else {
            schema.to_string()
        };
        let fq = Self::qualified_schema_table(&schema_mysql, mysql_table);
        let sql = format!(
            "SELECT * FROM {} WHERE {} = {} LIMIT 1",
            fq,
            Self::mysql_ident(pk_field),
            Self::mysql_string_literal(pk_val.trim())
        );
        let row_opt: Option<Row> = conn.query_first(sql).await?;
        match row_opt {
            Some(row) => Ok(Some(self.parse_row(row, qkey)?)),
            None => Ok(None),
        }
    }

    async fn detect_mariadb_server(conn: &mut Conn) -> bool {
        match conn.query_first::<String, _>("SELECT VERSION()").await {
            Ok(Some(v)) => v.to_lowercase().contains("mariadb"),
            _ => false,
        }
    }

    async fn global_gtid_mode_on(conn: &mut Conn) -> bool {
        match conn.query_first::<String, _>("SELECT @@GLOBAL.GTID_MODE").await {
            Ok(Some(mode)) => {
                let m = mode.trim();
                m.eq_ignore_ascii_case("ON") || m.starts_with("ON_PERMISSIVE")
            }
            _ => false,
        }
    }

    async fn maybe_capture_gtid_executed(
        &self,
        conn: &mut Conn,
        state: &mut CdcState,
        master_gtid_enabled: bool,
    ) -> Result<()> {
        if !master_gtid_enabled {
            return Ok(());
        }
        match conn.query_first::<String, _>("SELECT @@GLOBAL.gtid_executed").await {
            Ok(Some(gs)) => {
                let gs = gs.trim().to_string();
                if !gs.is_empty() {
                    state.gtid_executed = gs;
                    self.log_info(
                        "CDC: Stored @@GLOBAL.gtid_executed for GTID-aware binlog streaming.".into(),
                    );
                } else {
                    self.log_warn(
                        "CDC: @@GLOBAL.gtid_executed returned empty — GTID-aware streaming cannot activate; \
if binlog updates stall on RDS/Aurora, grant privileges to read gtid_executed or verify GTID configuration."
                            .to_string(),
                    );
                }
            }
            Err(e) => self.log_warn(format!(
                "CDC: @@GLOBAL.gtid_executed query failed ({}). GTID-aware binlog streaming disabled.",
                e
            )),
            Ok(None) => self.log_warn(
                "CDC: @@GLOBAL.gtid_executed returned NULL — GTID-aware streaming disabled.".to_string(),
            ),
        }
        Ok(())
    }

    fn split_gtid_executed_chunks(gtids: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for part in gtids.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            Sid::from_str(p).with_context(|| format!("invalid GTID fragment `{}`", p))?;
            out.push(p.to_string());
        }
        anyhow::ensure!(!out.is_empty(), "GTID set parsed to zero fragments");
        Ok(out)
    }

    /// Parses a comma-separated GTID set into half-open `[start,end)` intervals per source UUID.
    fn parse_gtid_executed_interval_map(s: &str) -> Result<HashMap<[u8; 16], Vec<(u64, u64)>>> {
        let mut map: HashMap<[u8; 16], Vec<(u64, u64)>> = HashMap::new();
        for part in s.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            let (uuid_bytes, mut intervals) =
                Self::parse_gtid_sid_block(p).with_context(|| format!("GTID fragment `{}`", p))?;
            map.entry(uuid_bytes)
                .or_default()
                .append(&mut intervals);
        }
        for ivs in map.values_mut() {
            Self::merge_half_open_intervals(ivs);
        }
        Ok(map)
    }

    /// One fragment like `uuid:gno` or `uuid:start-end:start2-end2`.
    fn parse_gtid_sid_block(block: &str) -> Result<([u8; 16], Vec<(u64, u64)>)> {
        let mut colon_parts = block.splitn(2, ':');
        let uuid_part = colon_parts.next().unwrap_or("");
        let rest = colon_parts.next().context("GTID fragment missing ':'")?;
        let u = Uuid::parse_str(uuid_part.trim()).context("GTID UUID")?;
        let uuid_bytes = *u.as_bytes();
        let mut intervals = Vec::new();
        for seg in rest.split(':') {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }
            let iv = if let Some((a, b)) = seg.split_once('-') {
                let start: u64 = a.trim().parse().context("GTID interval start")?;
                let end_inclusive: u64 = b.trim().parse().context("GTID interval end")?;
                anyhow::ensure!(start <= end_inclusive, "GTID inverted interval {}", seg);
                (start, end_inclusive.saturating_add(1))
            } else {
                let n: u64 = seg.parse().context("GTID gno")?;
                (n, n.saturating_add(1))
            };
            anyhow::ensure!(iv.0 > 0 && iv.1 > iv.0, "invalid GTID interval {:?}", iv);
            intervals.push(iv);
        }
        anyhow::ensure!(!intervals.is_empty(), "GTID fragment has no intervals");
        Ok((uuid_bytes, intervals))
    }

    fn merge_half_open_intervals(ivs: &mut Vec<(u64, u64)>) {
        if ivs.is_empty() {
            return;
        }
        ivs.sort_by_key(|x| x.0);
        let mut merged = Vec::with_capacity(ivs.len());
        let mut cur = ivs[0];
        for &(s, e) in ivs.iter().skip(1) {
            if s <= cur.1 {
                cur.1 = cur.1.max(e);
            } else {
                merged.push(cur);
                cur = (s, e);
            }
        }
        merged.push(cur);
        *ivs = merged;
    }

    fn format_gtid_executed_interval_map(map: &HashMap<[u8; 16], Vec<(u64, u64)>>) -> String {
        let mut keys: Vec<[u8; 16]> = map.keys().copied().collect();
        keys.sort_by_key(|k| Uuid::from_bytes(*k).as_u128());
        let mut parts = Vec::with_capacity(keys.len());
        for k in keys {
            let Some(intervals) = map.get(&k) else {
                continue;
            };
            if intervals.is_empty() {
                continue;
            }
            let uuid_str = Uuid::from_bytes(k).to_string();
            let mut block = uuid_str;
            for &(s, e) in intervals {
                block.push(':');
                if e == s.saturating_add(1) {
                    block.push_str(&s.to_string());
                } else {
                    block.push_str(&format!("{}-{}", s, e.saturating_sub(1)));
                }
            }
            parts.push(block);
        }
        parts.join(",")
    }

    /// Extends `gtid_executed` with one committed transaction (`sid`, `gno`) and merges intervals per server UUID.
    fn merge_gtid_executed_increment(existing: &str, sid_bytes: &[u8; 16], gno: u64) -> Result<String> {
        if gno == 0 {
            return Ok(existing.to_string());
        }
        let mut map = match Self::parse_gtid_executed_interval_map(existing.trim()) {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    "CDC: gtid_executed parse failed ({}); rebuilding set from streamed GTIDs.",
                    e
                );
                HashMap::new()
            }
        };
        let ivs = map.entry(*sid_bytes).or_default();
        ivs.push((gno, gno.saturating_add(1)));
        Self::merge_half_open_intervals(ivs);
        Ok(Self::format_gtid_executed_interval_map(&map))
    }

    async fn resolve_binlog_position(
        &self,
        conn: &mut Conn,
        state: &mut CdcState,
        master_gtid_enabled: bool,
    ) -> Result<()> {
        if !state.binlog_file.is_empty() {
            return Ok(());
        }

        match Self::query_master_coordinates(conn).await? {
            Some((file, pos)) => {
                state.binlog_file = file;
                state.binlog_pos = pos;
                self.log_info(format!(
                    "CDC: Real-time sync enabled. Starting from {} at position {}",
                    state.binlog_file, state.binlog_pos
                ));
                self.maybe_capture_gtid_executed(conn, state, master_gtid_enabled)
                    .await?;
            }
            None => {
                self.log_info(
                    "CDC: Operating in Static Data mode (Real-time updates not enabled on MySQL)."
                        .to_string(),
                );
            }
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

    /// Sync-all stores qkeys as lowercased `schema.table`; MySQL may use mixed-case schema names.
    /// Linux servers with `lower_case_table_names=0` reject `USE smartinvoicing` when the DB is `SmartInvoicing`.
    async fn resolve_mysql_schema_folder_name(conn: &mut Conn, schema_hint: &str) -> Result<String> {
        let rows: Vec<(String,)> = conn
            .exec(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
                 WHERE LOWER(SCHEMA_NAME) = LOWER(?) ORDER BY SCHEMA_NAME LIMIT 2",
                (schema_hint,),
            )
            .await
            .context("CDC: resolve schema via information_schema.SCHEMATA")?;
        match rows.len() {
            0 => anyhow::bail!(
                "CDC: no schema matching `{}` (case-insensitive) on server",
                schema_hint
            ),
            1 => Ok(rows.into_iter().next().unwrap().0),
            _ => anyhow::bail!(
                "CDC: ambiguous schema `{}`: multiple databases match case-insensitively",
                schema_hint
            ),
        }
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
        mode: BootstrapMode,
    ) -> Result<()> {
        let qkey = Self::qualified_table_key(self.sync_all_databases, schema, table_name);
        let disk_entity: String = if self.sync_all_databases {
            schema.to_lowercase()
        } else {
            self.entity.clone()
        };

        let schema_mysql = if self.sync_all_databases {
            Self::resolve_mysql_schema_folder_name(conn, schema).await?
        } else {
            schema.to_string()
        };

        self.log_info(format!(
            "CDC: Bootstrapping table '{}' (schema '{}' {:?})...",
            table_name, schema_mysql, mode
        ));

        let (cols, dates) = self
            .fetch_column_info(conn, &schema_mysql, &qkey, table_name)
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
            Self::mysql_string_literal(&schema_mysql),
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

        let mut count = 0usize;
        if mode == BootstrapMode::FullSnapshot {
            let fq = Self::qualified_schema_table(&schema_mysql, table_name);
            let mut result_set = conn.query_iter(format!("SELECT * FROM {}", fq)).await?;

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
            self.log_info(format!(
                "CDC: Table '{}' synchronized ({} rows).",
                qkey, count
            ));
        } else {
            self.log_info(format!(
                "CDC: Table '{}' registered without bulk snapshot — row data comes from binlog replay only.",
                qkey
            ));
        }

        table.flush_active_segment()?;
        Ok(())
    }

    fn wipe_bootstrapped_mirror_tables(&self, state: &CdcState) -> Result<()> {
        for qkey in &state.bootstrapped_tables {
            let (table_sql_name, disk_entity) = if self.sync_all_databases {
                let Some((s, t)) = qkey.split_once('.') else {
                    warn!("CDC: wipe mirror skipping malformed sync-all qkey '{}'", qkey);
                    continue;
                };
                (t.to_string(), s.to_lowercase())
            } else {
                (qkey.clone(), self.entity.clone())
            };
            let mirror_table = Self::resolve_mirror_table_dir(&disk_entity, &table_sql_name);
            let table_lock = match self.table_manager.get_table(&disk_entity, &mirror_table) {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        "CDC: wipe mirror skip '{}' (entity='{}' dir='{}'): {}",
                        qkey, disk_entity, mirror_table, e
                    );
                    continue;
                }
            };
            let mut table = table_lock.write().unwrap();
            table.delete_all_rows()?;
            debug!(
                "CDC: wiped local mirror rows for '{}' (entity='{}' dir='{}')",
                qkey, disk_entity, mirror_table
            );
        }
        Ok(())
    }

    /// After losing binlog replay position (purged logs / 1236), reload every bootstrapped table via `SELECT *`
    /// inside `START TRANSACTION WITH CONSISTENT SNAPSHOT`, matching bootstrap semantics so offline commits are not skipped.
    async fn resync_mirror_after_checkpoint_loss_mvcc(
        &self,
        pool: &Pool,
        state: &mut CdcState,
        _master_gtid_enabled: bool,
    ) -> Result<()> {
        self.log_warn(
            "CDC: Rebuilding mirrors after checkpoint loss — InnoDB snapshot + full SELECT * per bootstrapped table."
                .to_string(),
        );

        self.wipe_bootstrapped_mirror_tables(state)?;

        self.log_warn(
            "CDC: Mirrors for this profile were wiped — queries against local mirrors may return empty or stale rows until every bootstrapped table finishes SELECT * (can take a long time with sync-all)."
                .to_string(),
        );

        let mut conn = pool.get_conn().await?;
        if self.sync_all_databases {
            conn.query_drop("USE information_schema").await?;
        } else {
            conn.query_drop(format!("USE {}", Self::mysql_ident(&self.database)))
                .await?;
        }

        conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
            .await
            .context("CDC resync: START TRANSACTION WITH CONSISTENT SNAPSHOT")?;

        let coords = match Self::query_master_coordinates(&mut conn).await {
            Ok(Some((f, p))) if !f.is_empty() => (f, p),
            Ok(_) => {
                let _ = conn.query_drop("ROLLBACK").await;
                anyhow::bail!("CDC resync: empty binlog coordinates inside snapshot");
            }
            Err(e) => {
                let _ = conn.query_drop("ROLLBACK").await;
                return Err(e.context("CDC resync: SHOW MASTER/BINARY LOG STATUS failed"));
            }
        };

        let qkeys: Vec<String> = state.bootstrapped_tables.clone();
        let mut bootstrap_err: Option<(String, anyhow::Error)> = None;
        for qkey in qkeys {
            let (schema, table_name) = if self.sync_all_databases {
                match qkey.split_once('.') {
                    Some((s, t)) => (s.to_string(), t.to_string()),
                    None => {
                        warn!("CDC: resync skipping malformed sync-all qkey '{}'", qkey);
                        continue;
                    }
                }
            } else {
                (self.database.clone(), qkey.clone())
            };

            if let Err(e) = self
                .bootstrap_table(&mut conn, &schema, &table_name, state, BootstrapMode::FullSnapshot)
                .await
            {
                bootstrap_err = Some((qkey, e));
                break;
            }
        }

        if let Some((qkey, e)) = bootstrap_err {
            let _ = conn.query_drop("ROLLBACK").await;
            anyhow::bail!("CDC resync bootstrap failed for '{}': {}", qkey, e);
        }

        conn.query_drop("COMMIT").await.context("CDC resync COMMIT")?;

        state.binlog_file = coords.0;
        state.binlog_pos = coords.1;
        state.gtid_executed.clear();
        state.mvcc_snapshot_catchup_pending = true;

        Ok(())
    }

    fn resolve_rows_table_map(
        &self,
        rows_data: &RowsEventData<'_>,
        stream: &BinlogStream,
    ) -> Result<TableMapEvent<'static>> {
        let table_id = rows_data.table_id();
        stream
            .get_tme(table_id)
            .map(|t| t.clone().into_owned())
            .or_else(|| {
                self.table_map_events
                    .read()
                    .unwrap()
                    .get(&table_id)
                    .cloned()
            })
            .with_context(|| {
                format!(
                    "CDC: Missing TableMapEvent for table_id {} — cursor may resume mid-file before the table map for this event",
                    table_id
                )
            })
    }

    /// Tables created after the first snapshot (or missed earlier) appear in the binlog but not in `column_maps`.
    /// Register schema/PK via `information_schema` without `SELECT *` so replay can apply row events safely.
    async fn ensure_local_mirror_for_binlog_table(
        &self,
        conn: &mut Conn,
        state: &mut CdcState,
        schema_binlog: &str,
        table_binlog: &str,
    ) -> Result<()> {
        let maps_read = self.column_maps.read().unwrap();
        if Self::resolve_cdc_table_keys(
            self.sync_all_databases,
            schema_binlog,
            table_binlog,
            &maps_read,
        )
        .is_some()
        {
            return Ok(());
        }
        drop(maps_read);

        let rows: Vec<(String, String)> = conn
            .exec(
                "SELECT TABLE_SCHEMA, TABLE_NAME FROM information_schema.TABLES \
                 WHERE LOWER(TABLE_SCHEMA) = LOWER(?) AND LOWER(TABLE_NAME) = LOWER(?) \
                 AND TABLE_TYPE = 'BASE TABLE'",
                (schema_binlog, table_binlog),
            )
            .await?;

        let Some((sch, tbl)) = rows.into_iter().next() else {
            debug!(
                "CDC: Binlog referenced '{}.{}' but no BASE TABLE in information_schema (skipped).",
                schema_binlog, table_binlog
            );
            return Ok(());
        };

        if !self.is_table_in_sync_allowlist(&sch, &tbl) {
            return Ok(());
        }

        let qkey = Self::qualified_table_key(self.sync_all_databases, &sch, &tbl);
        if self.column_maps.read().unwrap().contains_key(&qkey) {
            return Ok(());
        }

        self.bootstrap_table(conn, &sch, &tbl, state, BootstrapMode::SchemaOnly)
            .await?;

        if !state.bootstrapped_tables.contains(&qkey) {
            state.bootstrapped_tables.push(qkey);
        }
        self.save_state(state)?;
        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        if let Err(e) = crate::core::data_paths::migrate_legacy_layout() {
            warn!(
                "CDC: migrate_legacy_layout failed ({:#}); profiles/mirror paths may be wrong for this process cwd.",
                e
            );
        }
        match self.run_impl().await {
            Ok(v) => Ok(v),
            Err(e) => {
                self.emit_startup_report(CdcStartupOutcome::Failed(format!("{:#}", e)));
                Err(e)
            }
        }
    }

    async fn run_impl(&self) -> Result<()> {
        {
            let loaded = Self::load_sync_table_allowlist_from_config(&self.entity);
            if let Some(ref s) = loaded {
                self.log_info(format!(
                    "CDC: `sync_tables` filter active — {} table(s).",
                    s.len()
                ));
            } else {
                self.log_info(
                    "CDC: no `sync_tables` allowlist — mirroring every table in scope \
(if you expected a filter: use a binary that supports `sync_tables` and ensure cdc_config.json is mounted)."
                        .to_string(),
                );
            }
            *self.sync_table_allowlist.write().unwrap() = loaded;
        }
        let data_root = crate::core::data_paths::resolved_data_root();
        let root_disp = std::fs::canonicalize(&data_root).unwrap_or_else(|_| data_root.clone());
        let state_pb = Path::new(&self.state_path);
        let state_disp = std::fs::canonicalize(state_pb).unwrap_or_else(|_| state_pb.to_path_buf());
        // Staged engine startup logs paths next to "Staged profile N/M" to avoid duplicate / ordering confusion.
        if self.startup_report_tx.is_none() {
            self.log_info(format!(
                "CDC: entity='{}' sync_all={} data_root={} cdc_state_path={}",
                self.entity,
                self.sync_all_databases,
                root_disp.display(),
                state_disp.display()
            ));
        }

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

        let raw_opts = match Opts::from_url(&final_url) {
            Ok(o) => o,
            Err(e) => {
                self.log_error(format!("Invalid URL: {}", e));
                return Err(e.into());
            }
        };
        let health_host = raw_opts.ip_or_hostname().to_string();
        let health_port = raw_opts.tcp_port();
        let opts_with_keepalive: Opts = OptsBuilder::from_opts(raw_opts.clone())
            .tcp_keepalive(Some(30u32))
            .into();
        let opts_without_keepalive: Opts = OptsBuilder::from_opts(raw_opts).into();
        let mut pool = Pool::new(opts_with_keepalive);
        self.log_info(format!("CDC: Connecting to MySQL at {}...", final_url.split('@').last().unwrap_or("unknown")));

        let mut conn = match tokio::time::timeout(std::time::Duration::from_secs(30), pool.get_conn()).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                let msg = e.to_string();
                if msg.contains("os error 22") || msg.contains("Invalid argument") {
                    self.log_warn(
                        "CDC: MySQL connect failed with TCP keepalive enabled (os error 22). Retrying without keepalive."
                            .to_string(),
                    );
                    pool = Pool::new(opts_without_keepalive);
                    match tokio::time::timeout(std::time::Duration::from_secs(30), pool.get_conn()).await {
                        Ok(Ok(c2)) => c2,
                        Ok(Err(e2)) => {
                            self.log_error(format!("CDC: Connection failed after keepalive fallback: {}", e2));
                            return Err(e2.into());
                        }
                        Err(_) => {
                            self.log_error(
                                "CDC: Connection timed out after 30 seconds (after keepalive fallback). Check network/firewall."
                                    .to_string(),
                            );
                            return Err(anyhow::anyhow!("Connection timeout"));
                        }
                    }
                } else {
                    self.log_error(format!("CDC: Connection failed: {}", e));
                    return Err(e.into());
                }
            }
            Err(_) => {
                self.log_error("CDC: Connection timed out after 30 seconds. Check network/firewall.".to_string());
                return Err(anyhow::anyhow!("Connection timeout"));
            }
        };

        self.log_info("CDC: Successfully connected. Checking Binlog status...".to_string());
        let d = crate::core::cdc_durability::durability();
        let n = crate::core::cdc_durability::mirror_flush_every_n_batches();
        let wal = crate::core::cdc_durability::wal_sync_each_append();
        self.log_info(format!(
            "CDC: durability profile={:?} (mirror_flush_every_n_batches={n}, wal_sync_each_append={wal}); \
set BITTICE_CDC_DURABILITY=strict|balanced|throughput",
            d
        ));
        self.log_phase(1, "Connection, scope, and binlog policy");

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

        let server_is_mariadb = Self::detect_mariadb_server(&mut conn).await;
        let master_gtid_enabled = !server_is_mariadb && Self::global_gtid_mode_on(&mut conn).await;

        let mut state = self.load_state();
        // If `cdc_state.json` already had binlog coordinates when this worker started, never substitute
        // `@@GLOBAL.gtid_executed` into an empty `gtid_executed` — the server would skip transactions the mirror
        // never applied (data loss after restarts).
        let had_checkpoint_at_start = !state.binlog_file.is_empty();
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

        match conn.query_first::<String, _>("SELECT @@GLOBAL.binlog_format").await {
            Ok(Some(fmt)) => {
                let trimmed = fmt.trim().to_string();
                state.observed_binlog_format_global = Some(trimmed.clone());
                self.log_info(format!("CDC: MySQL @@GLOBAL.binlog_format='{}'.", trimmed));
                if trimmed.to_ascii_lowercase() != "row" {
                    self.log_error(format!(
                        "CDC: @@GLOBAL.binlog_format is '{}' — Bittice CANNOT mirror most transactional INSERT/UPDATE/DELETE. \
The binlog stream uses statement/mixed events for many writes; this engine only applies row events (WRITE/UPDATE/DELETE_ROWS). \
Fix on MySQL/RDS: set parameter binlog_format=ROW (custom DB parameter group on RDS; apply to instance/cluster; reboot if required). \
Then restart Bittice.",
                        trimmed
                    ));
                }
            }
            Ok(None) | Err(_) => {
                self.log_warn(
                    "CDC: Could not read @@GLOBAL.binlog_format; if it is not ROW, changes may not replicate."
                        .to_string(),
                );
            }
        }
        match conn.query_first::<String, _>("SELECT @@SESSION.binlog_format").await {
            Ok(Some(fmt)) => {
                let trimmed = fmt.trim().to_string();
                state.observed_binlog_format_session = Some(trimmed.clone());
                self.log_info(format!("CDC: MySQL @@SESSION.binlog_format='{}'.", trimmed));
                if trimmed.to_ascii_lowercase() != "row" {
                    self.log_warn(format!(
                        "CDC: @@SESSION.binlog_format is '{}' — apps may override session format; global binlog_format must still be ROW for reliable CDC.",
                        trimmed
                    ));
                }
            }
            Ok(None) | Err(_) => {}
        }
        let _ = self.save_state(&state);

        self.log_phase(2, "Mirror snapshot (discovery and initial copy)");

        // InnoDB-only: single MVCC snapshot + binlog coords **before** bulk `SELECT *`, matching mysqldump `--single-transaction` semantics.
        let requires_lossless_mvcc_bootstrap = state.binlog_file.is_empty();
        let mut rr_snapshot_active = false;
        let mut coords_pre_bootstrap: Option<(String, u32)> = None;

        if state.binlog_file.is_empty() {
            match conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT").await {
                Ok(()) => {
                    rr_snapshot_active = true;
                    match Self::query_master_coordinates(&mut conn).await {
                        Ok(Some((f, p))) if !f.is_empty() => {
                            coords_pre_bootstrap = Some((f.clone(), p));
                            self.log_info(format!(
                                "CDC: InnoDB consistent snapshot active; captured binlog {}:{} before bulk table copy.",
                                f, p
                            ));
                        }
                        Ok(Some(_)) | Ok(None) => {
                            self.log_warn(
                                "CDC: SHOW MASTER STATUS returned empty coordinates inside snapshot; binlog tip will be resolved after bootstrap."
                                    .to_string(),
                            );
                        }
                        Err(e) => {
                            self.log_warn(format!(
                                "CDC: Could not read binlog coordinates inside snapshot ({}); tip resolved after bootstrap.",
                                e
                            ));
                        }
                    }
                }
                Err(e) => {
                    self.log_warn(format!(
                        "CDC: START TRANSACTION WITH CONSISTENT SNAPSHOT failed ({}): sequential snapshot without MVCC alignment; binlog tip resolved after bootstrap.",
                        e
                    ));
                }
            }
        }

        if requires_lossless_mvcc_bootstrap {
            if !rr_snapshot_active {
                return self
                    .enter_static_mode(
                        "CDC: Lossless initial sync requires START TRANSACTION WITH CONSISTENT SNAPSHOT (InnoDB). \
The server rejected it or the session could not open a snapshot. Use a primary/writer (not a read replica) and InnoDB tables."
                            .to_string(),
                    )
                    .await;
            }
            if coords_pre_bootstrap.is_none() {
                Self::rollback_consistent_snapshot(&mut conn, true).await;
                return self
                    .enter_static_mode(
                        "CDC: Lossless initial sync requires binary log coordinates inside the consistent snapshot \
(SHOW BINARY LOG STATUS or SHOW MASTER STATUS). Enable binary logging on MySQL/RDS and grant REPLICATION CLIENT (or equivalent) \
so the account can read coordinates."
                            .to_string(),
                    )
                    .await;
            }
        }

        if self.sync_all_databases {
            let schemas = match Self::list_user_schemas(&mut conn).await {
                Ok(s) => s,
                Err(e) => {
                    Self::rollback_consistent_snapshot(&mut conn, rr_snapshot_active).await;
                    self.log_error(format!("Failed to list databases: {}", e));
                    return Err(e.into());
                }
            };
            self.log_info(format!(
                "CDC: Discovered {} user database(s) to sync.",
                schemas.len()
            ));
            for schema in schemas {
                if crate::server::engine_halt_requested() {
                    self.log_info(format!(
                        "CDC: Worker '{}' snapshot interrupted (engine shutdown).",
                        self.entity
                    ));
                    self.fast_exit_flush();
                    let _ = self.save_state(&state);
                    return Ok(());
                }
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
                    if crate::server::engine_halt_requested() {
                        self.log_info(format!(
                            "CDC: Worker '{}' snapshot interrupted (engine shutdown).",
                            self.entity
                        ));
                        self.fast_exit_flush();
                        let _ = self.save_state(&state);
                        return Ok(());
                    }
                    if !self.is_table_in_sync_allowlist(&schema, table_name) {
                        continue;
                    }
                    let qkey = Self::qualified_table_key(true, &schema, table_name);
                    if !state.bootstrapped_tables.contains(&qkey) {
                        if let Err(e) = self
                            .bootstrap_table(&mut conn, &schema, table_name, &mut state, BootstrapMode::FullSnapshot)
                            .await
                        {
                            Self::rollback_consistent_snapshot(&mut conn, rr_snapshot_active).await;
                            return self
                                .enter_static_mode(format!(
                                    "Bootstrap failed for '{}': {}.",
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
                                Self::rollback_consistent_snapshot(&mut conn, rr_snapshot_active).await;
                                return self
                                    .enter_static_mode(format!(
                                        "Failed to refresh schema for '{}': {}.",
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
                    Self::rollback_consistent_snapshot(&mut conn, rr_snapshot_active).await;
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
                if crate::server::engine_halt_requested() {
                    self.log_info(format!(
                        "CDC: Worker '{}' snapshot interrupted (engine shutdown).",
                        self.entity
                    ));
                    self.fast_exit_flush();
                    let _ = self.save_state(&state);
                    return Ok(());
                }
                if !self.is_table_in_sync_allowlist(self.database.as_str(), table_name) {
                    continue;
                }
                let qkey = Self::qualified_table_key(false, &self.database, table_name);
                if !state.bootstrapped_tables.contains(&qkey) {
                    if let Err(e) = self
                        .bootstrap_table(&mut conn, self.database.as_str(), table_name, &mut state, BootstrapMode::FullSnapshot)
                        .await
                    {
                        Self::rollback_consistent_snapshot(&mut conn, rr_snapshot_active).await;
                            return self
                                .enter_static_mode(format!(
                                    "Bootstrap failed for '{}': {}.",
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
                            Self::rollback_consistent_snapshot(&mut conn, rr_snapshot_active).await;
                            return self
                                .enter_static_mode(format!(
                                    "Failed to refresh schema for '{}': {}.",
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

        if rr_snapshot_active {
            if let Err(e) = conn.query_drop("COMMIT").await {
                self.log_error(format!(
                    "CDC: COMMIT after consistent snapshot bootstrap failed: {}",
                    e
                ));
                return Err(e.into());
            }
        }

        self.log_phase(3, "Replication checkpoint (binlog position and safety checks)");

        // After `START TRANSACTION WITH CONSISTENT SNAPSHOT`, binlog file/pos were taken at the **start** of the bulk
        // `SELECT *` pass. Commits on any table during that window must be replayed from the binlog. If we instead
        // seed `gtid_executed` from `@@GLOBAL.gtid_executed` **after** the copy, the server assumes we already applied
        // those GTIDs and skips events — rows can disappear from the mirror while MySQL has them.
        let mut replay_binlog_backlog_after_mvcc =
            rr_snapshot_active && coords_pre_bootstrap.is_some();

        if rr_snapshot_active
            && coords_pre_bootstrap.is_some()
            && Self::env_truthy("BITTICE_CDC_SKIP_BINLOG_BACKLOG")
        {
            if !Self::env_truthy("BITTICE_CDC_ACCEPT_SNAPSHOT_GAP") {
                self.log_warn(
                    "CDC: BITTICE_CDC_SKIP_BINLOG_BACKLOG ignored — lossless bootstrap replays the binlog from the snapshot coordinates. \
Set BITTICE_CDC_ACCEPT_SNAPSHOT_GAP=1 together with SKIP to jump to the current tip (commits during the snapshot window can be missing)."
                        .to_string(),
                );
            } else {
                match Self::query_master_coordinates(&mut conn).await {
                    Ok(Some((f, p))) if !f.is_empty() => {
                        coords_pre_bootstrap = Some((f, p));
                        replay_binlog_backlog_after_mvcc = false;
                        self.log_warn(
                            "CDC: BITTICE_CDC_SKIP_BINLOG_BACKLOG + ACCEPT_SNAPSHOT_GAP — streaming from current binlog tip after bootstrap. \
Commits during the bulk snapshot window may be missing."
                                .to_string(),
                        );
                    }
                    Ok(_) | Err(_) => {
                        self.log_warn(
                            "CDC: SKIP_BINLOG_BACKLOG set but tip coordinates unreadable; keeping snapshot coordinates."
                                .to_string(),
                        );
                    }
                }
            }
        }

        if let Some((f, p)) = coords_pre_bootstrap.take() {
            state.binlog_file = f;
            state.binlog_pos = p;
            if replay_binlog_backlog_after_mvcc {
                state.gtid_executed.clear();
                state.mvcc_snapshot_catchup_pending = true;
                self.log_info(
                    "CDC: MVCC bootstrap — replaying from snapshot binlog coordinates with empty gtid_executed \
so writes during the bulk copy are not skipped by GTID; gtid_executed is rebuilt from streamed GtidEvent(s)."
                        .to_string(),
                );
            } else {
                state.mvcc_snapshot_catchup_pending = false;
                self.maybe_capture_gtid_executed(&mut conn, &mut state, master_gtid_enabled)
                    .await?;
            }
            let _ = self.save_state(&state);
        }

        self.resolve_binlog_position(&mut conn, &mut state, master_gtid_enabled)
            .await?;

        if state.binlog_file.is_empty() {
            return self.enter_static_mode("CDC is not enabled on server.".to_string()).await;
        }

        // RDS/Aurora: seeding from @@GLOBAL.gtid_executed is only safe before any replication coordinates exist on disk.
        // If we resumed with a saved `binlog_file`/`binlog_pos` but empty `gtid_executed`, file/position mode replays
        // correctly; copying the server's full GTID set would skip transactions the mirror never received.
        if master_gtid_enabled
            && state.gtid_executed.trim().is_empty()
            && !replay_binlog_backlog_after_mvcc
            && !had_checkpoint_at_start
        {
            self.log_warn(
                "CDC: GTID_MODE=ON but saved gtid_executed is empty — fetching @@GLOBAL.gtid_executed before binlog stream."
                    .to_string(),
            );
            self.maybe_capture_gtid_executed(&mut conn, &mut state, master_gtid_enabled)
                .await?;
            let _ = self.save_state(&state);
        }
        if master_gtid_enabled && state.gtid_executed.trim().is_empty() && had_checkpoint_at_start {
            self.log_info(
                "CDC: Resuming with empty gtid_executed — using filename/position replication only until the stream repopulates GTIDs (avoids skipping events vs @@GLOBAL.gtid_executed)."
                    .to_string(),
            );
        }

        let global_binlog_is_row = state
            .observed_binlog_format_global
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("row"));
        if !global_binlog_is_row {
            if Self::env_truthy("BITTICE_CDC_REQUIRE_ROW") {
                return self
                    .enter_static_mode(
                        "CDC: @@GLOBAL.binlog_format is not ROW and BITTICE_CDC_REQUIRE_ROW=1. \
Set binlog_format=ROW on the RDS/MySQL instance (parameter group), reboot if needed, then restart Bittice — or unset BITTICE_CDC_REQUIRE_ROW to run with incomplete live sync."
                            .to_string(),
                    )
                    .await;
            }
            self.log_warn(
                "CDC: Live transactional sync is PARTIAL or INACTIVE for typical writes — @@GLOBAL.binlog_format is not ROW. \
Queries may show stale data vs MySQL. Set BITTICE_CDC_REQUIRE_ROW=1 to refuse streaming until the server is fixed."
                    .to_string(),
            );
        }

        if self.sync_all_databases {
            self.log_info(
                "CDC: Sync-all mode".to_string(),
            );
        }

        // Resume-with-gap detection: when we have a saved binlog position from a previous session
        // and that position is behind the current master tip, events that occurred while Bittice was
        // offline (e.g. the user went to the main menu to connect a new RDS) must be replayed before
        // the engine is considered "live".  Setting mvcc_snapshot_catchup_pending here re-uses the
        // existing backlog-drain mechanism so that:
        //   • run_cdc_staged_sequential waits for the catchup to finish before opening HTTP,
        //   • tables are flushed durably once the gap is closed.
        // We only do this check when NOT already pending (fresh bootstrap sets it independently).
        if !state.mvcc_snapshot_catchup_pending
            && had_checkpoint_at_start
            && !requires_lossless_mvcc_bootstrap
        {
            match self.query_master_coordinates_pool(&pool).await {
                Ok(Some((tip_file, tip_pos))) if !tip_file.is_empty() => {
                    if !Self::cdc_replication_caught_up_to_tip(&state, &tip_file, tip_pos) {
                        state.mvcc_snapshot_catchup_pending = true;
                        self.log_info(format!(
                            "CDC: [{}] Resume gap detected — saved {}:{} is behind master {}:{}; \
replaying missed events before declaring live (data added while Bittice was offline will be recovered).",
                            self.entity,
                            state.binlog_file, state.binlog_pos,
                            tip_file, tip_pos,
                        ));
                        let _ = self.save_state(&state);
                    }
                }
                _ => {}
            }
        }

        let pool = pool.clone();
        let mut last_state_save = std::time::Instant::now();
        let mut events_since_save: u32 = 0;
        let mut live_startup_reported = false;

        'binlog_retry: loop {
            if crate::server::engine_halt_requested() {
                self.log_info(format!(
                    "CDC: Worker '{}' stopping before binlog stream (engine shutdown).",
                    self.entity
                ));
                self.fast_exit_flush();
                let _ = self.save_state(&state);
                return Ok(());
            }
            if Self::env_truthy("BITTICE_CDC_LOG_ROW_EVENTS")
                && !CDC_ROW_TRACE_BANNER_EMITTED.swap(true, Ordering::SeqCst)
            {
                self.log_info(
                    "CDC: BITTICE_CDC_LOG_ROW_EVENTS=1 — logging mysql→mirror INSERT/UPDATE/DELETE batches (unset to silence)."
                        .to_string(),
                );
            }

            conn = pool.get_conn().await?;
            if self.sync_all_databases {
                conn.query_drop("USE information_schema").await?;
            } else {
                conn.query_drop(format!("USE {}", Self::mysql_ident(&self.database)))
                    .await?;
            }

            if state.binlog_file.is_empty() {
                self.resolve_binlog_position(&mut conn, &mut state, master_gtid_enabled)
                    .await?;
                if state.binlog_file.is_empty() {
                    return self
                        .enter_static_mode(
                            "CDC: Could not resolve binlog coordinates for streaming.".to_string(),
                        )
                        .await;
                }
            }

            self.log_info(format!(
                "CDC: Resuming live stream from {}:{}",
                state.binlog_file, state.binlog_pos
            ));

            let server_id = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                % 100_000) as u32
                + 1000;

            let filename_b = state.binlog_file.as_bytes();
            let pos_u64 = state.binlog_pos as u64;

            let use_gtid_dump = master_gtid_enabled && !state.gtid_executed.trim().is_empty();

            let binlog_open_result = if use_gtid_dump {
                match Self::split_gtid_executed_chunks(state.gtid_executed.trim()) {
                    Ok(chunk_strings) => {
                        let sids: Vec<Sid<'_>> = chunk_strings
                            .iter()
                            .map(|c| Sid::from_str(c.as_str()).expect("GTID fragment pre-validated"))
                            .collect();
                        conn.get_binlog_stream(
                            BinlogStreamRequest::new(server_id)
                                .with_gtid()
                                .with_filename(filename_b)
                                .with_pos(pos_u64)
                                .with_gtid_set(sids.into_iter()),
                        )
                        .await
                    }
                    Err(e) => {
                        warn!(
                            "CDC: Stored gtid_executed could not be parsed ({}); opening binlog stream without GTID.",
                            e
                        );
                        conn.get_binlog_stream(
                            BinlogStreamRequest::new(server_id)
                                .with_filename(filename_b)
                                .with_pos(pos_u64),
                        )
                        .await
                    }
                }
            } else {
                if master_gtid_enabled && state.gtid_executed.trim().is_empty() {
                    debug!(
                        "CDC: GTID_MODE is ON but gtid_executed snapshot empty; using filename/position replication."
                    );
                }
                conn.get_binlog_stream(
                    BinlogStreamRequest::new(server_id)
                        .with_filename(filename_b)
                        .with_pos(pos_u64),
                )
                .await
            };

            let mut stream = match binlog_open_result {
                Ok(s) => s,
                Err(e) => {
                    let msg = e.to_string();
                    if Self::is_stale_saved_binlog_error(&msg) {
                        warn!(
                            "CDC: Saved binlog checkpoint no longer on server (purged or rotated; {}). {}",
                            msg,
                            if Self::cdc_mirror_resync_on_binlog_gap_enabled()
                                && !state.bootstrapped_tables.is_empty()
                            {
                                "Attempting MVCC mirror rebuild so offline commits are not skipped."
                            } else {
                                "Discarding checkpoint and anchoring to binlog tip (mirror may omit rows changed while Bittice was offline)."
                            }
                        );
                        if Self::cdc_mirror_resync_on_binlog_gap_enabled()
                            && !state.bootstrapped_tables.is_empty()
                        {
                            match self
                                .resync_mirror_after_checkpoint_loss_mvcc(
                                    &pool,
                                    &mut state,
                                    master_gtid_enabled,
                                )
                                .await
                            {
                                Ok(()) => {
                                    let _ = self.save_state(&state);
                                    self.log_info(
                                        "CDC: Mirror rebuilt after checkpoint loss; streaming from MVCC-aligned binlog coordinates."
                                            .to_string(),
                                    );
                                    continue 'binlog_retry;
                                }
                                Err(e) => {
                                    self.log_warn(format!(
                                        "CDC: MVCC mirror rebuild failed ({:#}); falling back to tip anchor only.",
                                        e
                                    ));
                                }
                            }
                        }
                        state.binlog_file.clear();
                        state.binlog_pos = 0;
                        state.gtid_executed.clear();
                        self.save_state(&state)?;
                        continue 'binlog_retry;
                    }
                    let _ = self.save_state(&state);
                    return self
                        .enter_static_mode(format!("Binlog stream unavailable: {}.", msg))
                        .await;
                }
            };

            let needs_catchup_before_live =
                state.mvcc_snapshot_catchup_pending || self.exit_after_cdc_ready;
            if !needs_catchup_before_live && !live_startup_reported {
                live_startup_reported = true;
                self.log_phase(4, "Live binlog stream (real-time replication)");
                self.emit_startup_report(CdcStartupOutcome::LiveReplication);
            }

            let mut repl_caught_up_streak: u32 = 0;
            let health_check_interval = cdc_health_check_interval();
            let mut last_health_check = std::time::Instant::now();
            let max_health_check_failures = std::env::var("BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(3);
            let infinite_health_retries = max_health_check_failures == 0;
            let mut consecutive_health_failures: u32 = 0;
            let vpn_restart_cooldown = std::time::Duration::from_secs(
                std::env::var("BITTICE_CDC_VPN_RESTART_COOLDOWN_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(20),
            );
            let mut last_vpn_restart_attempt: Option<std::time::Instant> = None;
            loop {
                if crate::server::engine_halt_requested() {
                    self.log_info(format!(
                        "CDC: Worker '{}' leaving live binlog stream (engine shutdown).",
                        self.entity
                    ));
                    self.fast_exit_flush();
                    let _ = self.save_state(&state);
                    return Ok(());
                }
                let next_item = tokio::select! {
                    biased;
                    opt = stream.next() => opt,
                    _ = tokio::time::sleep(CDC_ENGINE_HALT_POLL) => {
                        if self.exit_after_cdc_ready || state.mvcc_snapshot_catchup_pending {
                            match self.query_master_coordinates_pool(&pool).await {
                                Ok(Some((mf, mp))) if !mf.is_empty() => {
                                    if Self::cdc_replication_caught_up_to_tip(&state, mf.as_str(), mp) {
                                        repl_caught_up_streak =
                                            repl_caught_up_streak.saturating_add(1);
                                        if repl_caught_up_streak >= 2 {
                                            state.mvcc_snapshot_catchup_pending = false;
                                            let _ = self.save_state(&state);
                                            if self.exit_after_cdc_ready {
                                                // Flush all open mirror tables so binlog rows applied
                                                // during the snapshot catchup window (e.g. inserts that
                                                // arrived while the bulk SELECT * was running) are
                                                // durably written to disk before this process exits.
                                                // Without this, the active segment, primary.idx, and
                                                // bitmaps are never persisted and the server restart
                                                // silently loses those rows.
                                                self.table_manager.flush_dirty_tables();
                                                if let Some(tx) = &self.log_tx {
                                                    let _ = tx.try_send("CDC_READY".to_string());
                                                }
                                                self.log_info(
                                                    "CDC: Snapshot backlog drained — mirror coordinates caught up to master tip before CLI handoff."
                                                        .to_string(),
                                                );
                                                return Ok(());
                                            }
                                            if !live_startup_reported {
                                                // Flush all open mirror tables durably so that
                                                // primary.idx and bitmaps reflect the replayed rows
                                                // before HTTP requests can read them.
                                                self.table_manager.flush_dirty_tables();
                                                live_startup_reported = true;
                                                self.log_phase(
                                                    4,
                                                    "Live binlog stream (real-time replication)",
                                                );
                                                self.emit_startup_report(CdcStartupOutcome::LiveReplication);
                                            }
                                            repl_caught_up_streak = 0;
                                        }
                                    } else {
                                        repl_caught_up_streak = 0;
                                    }
                                }
                                _ => {}
                            }
                        }
                        if last_health_check.elapsed() >= health_check_interval {
                            last_health_check = std::time::Instant::now();
                            let addr = (health_host.as_str(), health_port);
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tokio::net::TcpStream::connect(addr),
                            )
                            .await
                            {
                                Ok(Ok(_)) => {
                                    if consecutive_health_failures > 0 {
                                        self.log_info(format!(
                                            "CDC: MySQL route recovered after {} transient health-check failure(s).",
                                            consecutive_health_failures
                                        ));
                                        consecutive_health_failures = 0;
                                    }
                                }
                                _ => {
                                    consecutive_health_failures =
                                        consecutive_health_failures.saturating_add(1);
                                    let can_restart_vpn = last_vpn_restart_attempt
                                        .map(|t| t.elapsed() >= vpn_restart_cooldown)
                                        .unwrap_or(true);
                                    if can_restart_vpn
                                        && self.try_restart_openvpn_for_route_loss(&health_host)
                                    {
                                        last_vpn_restart_attempt = Some(std::time::Instant::now());
                                    }
                                    if infinite_health_retries {
                                        self.log_warn(format!(
                                            "CDC: transient MySQL route failure #{} for {}:{}; retrying (infinite mode).",
                                            consecutive_health_failures,
                                            health_host,
                                            health_port,
                                        ));
                                    } else if consecutive_health_failures < max_health_check_failures {
                                        self.log_warn(format!(
                                            "CDC: transient MySQL route failure {}/{} for {}:{}; retrying before shutdown.",
                                            consecutive_health_failures,
                                            max_health_check_failures,
                                            health_host,
                                            health_port,
                                        ));
                                    } else {
                                        let _ = self.save_state(&state);
                                        return self
                                            .enter_static_mode(format!(
                                                "Lost network route to MySQL host {}:{}",
                                                health_host, health_port,
                                            ))
                                            .await;
                                    }
                                }
                            }
                        }
                        continue;
                    }
                };
                let Some(event) = next_item else {
                    break;
                };
                let event = match event {
                    Ok(e) => {
                        if self.exit_after_cdc_ready || state.mvcc_snapshot_catchup_pending {
                            repl_caught_up_streak = 0;
                        }
                        e
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if Self::is_stale_saved_binlog_error(&msg) {
                            warn!(
                                "CDC: Binlog stream interrupted — checkpoint stale or unavailable ({}). {}",
                                msg,
                                if Self::cdc_mirror_resync_on_binlog_gap_enabled()
                                    && !state.bootstrapped_tables.is_empty()
                                {
                                    "Attempting MVCC mirror rebuild."
                                } else {
                                    "Discarding checkpoint and anchoring to binlog tip."
                                }
                            );
                            if Self::cdc_mirror_resync_on_binlog_gap_enabled()
                                && !state.bootstrapped_tables.is_empty()
                            {
                                match self
                                    .resync_mirror_after_checkpoint_loss_mvcc(
                                        &pool,
                                        &mut state,
                                        master_gtid_enabled,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        let _ = self.save_state(&state);
                                        self.log_info(
                                            "CDC: Mirror rebuilt after checkpoint loss; streaming from MVCC-aligned binlog coordinates."
                                                .to_string(),
                                        );
                                        continue 'binlog_retry;
                                    }
                                    Err(e) => {
                                        self.log_warn(format!(
                                            "CDC: MVCC mirror rebuild failed ({:#}); falling back to tip anchor only.",
                                            e
                                        ));
                                    }
                                }
                            }
                            state.binlog_file.clear();
                            state.binlog_pos = 0;
                            state.gtid_executed.clear();
                            self.save_state(&state)?;
                            continue 'binlog_retry;
                        }
                        let _ = self.save_state(&state);
                        return self
                            .enter_static_mode(format!("Binlog stream error: {}.", msg))
                            .await;
                    }
                };
                let header = event.header();
                let next_pos = header.log_pos();

                let data = event.read_data()?;
                let checkpoint_rotate =
                    matches!(&data, Some(EventData::RotateEvent(_)));

                match data {
                    Some(EventData::GtidEvent(ev)) if master_gtid_enabled => {
                        match Self::merge_gtid_executed_increment(
                            state.gtid_executed.as_str(),
                            &ev.sid(),
                            ev.gno(),
                        ) {
                            Ok(merged) => state.gtid_executed = merged,
                            Err(e) => warn!("CDC: gtid_executed merge failed: {}", e),
                        }
                        Self::apply_binlog_file_pos(&mut state, next_pos);
                    }
                    Some(EventData::TableMapEvent(tm)) => {
                        let mut map = self.table_map_events.write().unwrap();
                        map.insert(tm.table_id(), tm.into_owned());
                        Self::apply_binlog_file_pos(&mut state, next_pos);
                    }
                    Some(EventData::RowsEvent(rows_data)) => {
                        if let Ok(tm_probe) = self.resolve_rows_table_map(&rows_data, &stream) {
                            let schema = tm_probe.database_name().to_string();
                            let tbl = tm_probe.table_name().to_string();
                            if !(self.sync_all_databases && Self::is_system_schema(&schema))
                                && !self.is_out_of_scope_binlog_schema(&schema)
                                && self.is_table_in_sync_allowlist(&schema, &tbl)
                            {
                                let known = {
                                    let maps = self.column_maps.read().unwrap();
                                    Self::resolve_cdc_table_keys(
                                        self.sync_all_databases,
                                        &schema,
                                        &tbl,
                                        &maps,
                                    )
                                    .is_some()
                                };
                                if !known {
                                    let mut extra = pool.get_conn().await?;
                                    if self.sync_all_databases {
                                        extra.query_drop("USE information_schema").await?;
                                    } else {
                                        extra
                                            .query_drop(format!(
                                                "USE {}",
                                                Self::mysql_ident(&self.database)
                                            ))
                                            .await?;
                                    }
                                    if let Err(e) = self
                                        .ensure_local_mirror_for_binlog_table(
                                            &mut extra,
                                            &mut state,
                                            &schema,
                                            &tbl,
                                        )
                                        .await
                                    {
                                        warn!(
                                            "CDC: Could not register '{}.{}' from binlog ({})",
                                            schema, tbl, e
                                        );
                                    }
                                }
                            }
                        }
                        self.handle_rows_event(&pool, rows_data, &mut state, &stream).await?;
                        Self::apply_binlog_file_pos(&mut state, next_pos);
                    }
                    Some(EventData::RotateEvent(rotate_data)) => {
                        if !rotate_data.is_fake() {
                            self.table_map_events.write().unwrap().clear();
                        }
                        state.binlog_file = rotate_data.name().to_string();
                        state.binlog_pos = rotate_data.position() as u32;
                    }
                    _ => {
                        Self::apply_binlog_file_pos(&mut state, next_pos);
                    }
                }

                events_since_save = events_since_save.saturating_add(1);
                let periodic_save = last_state_save.elapsed() >= CDC_STATE_SAVE_INTERVAL
                    || events_since_save >= CDC_STATE_SAVE_EVENT_BURST;
                if checkpoint_rotate || periodic_save {
                    self.flush_all_mirror_tables();
                    self.save_state(&state)?;
                    events_since_save = 0;
                    last_state_save = std::time::Instant::now();
                }
            }

            self.save_state(&state)?;
            return self
                .enter_static_mode(format!(
                    "CDC: Binlog stream ended unexpectedly for entity '{}'.",
                    self.entity
                ))
                .await;
        }
    }

    fn apply_binlog_write_rows<'a>(
        &self,
        rows: RowsEventRows<'a>,
        qkey: &str,
        disk_entity: &str,
        table_name: &str,
        _pk_field: &str,
        table: &mut Table,
    ) -> Result<usize> {
        debug!("CDC: Received Write event for table '{}'", qkey);
        let mut applied = 0usize;
        for row_pair in rows {
            let Ok((before_opt, after_opt)) = row_pair else {
                continue;
            };
            let Some(binlog_row) = after_opt.or(before_opt) else {
                continue;
            };
            let row = Row::try_from(binlog_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
            let data = self.parse_row(row, qkey)?;
            table.insert(data.clone())?;
            applied += 1;
        }
        if applied > 0 {
            let _ = self.table_manager.events_tx.send(TableUpdateEvent {
                entity: disk_entity.to_string(),
                table_name: table_name.to_string(),
                event_type: "INSERT".to_string(),
                pk: String::new(),
                row: vec![applied.to_string()],
            });
        }
        Ok(applied)
    }

    fn extract_binlog_update_deltas<'a>(
        &self,
        rows: RowsEventRows<'a>,
        qkey: &str,
        mirror_table: &str,
        pk_field: &str,
    ) -> Result<Vec<(String, HashMap<String, String>)>> {
        let mut deltas = Vec::new();
        for row_pair in rows {
            let Ok((before_opt, after_opt)) = row_pair else {
                continue;
            };
            let Some(after_row) = after_opt else {
                continue;
            };
            let mut delta =
                self.parse_row(Row::try_from(after_row).map_err(|e| anyhow::anyhow!("{:?}", e))?, qkey)?;

            // With `binlog_row_image=MINIMAL` (RDS default), UPDATE after-images list only columns that
            // changed; unchanged primary-key columns live in the before-image only.
            if let Some(before_row) = before_opt {
                let before_map =
                    self.parse_row(Row::try_from(before_row).map_err(|e| anyhow::anyhow!("{:?}", e))?, qkey)?;
                let pk_missing_or_empty = delta
                    .get(pk_field)
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true);
                if pk_missing_or_empty {
                    if let Some(pk_val) = before_map.get(pk_field).cloned().filter(|s| !s.trim().is_empty()) {
                        delta.insert(pk_field.to_string(), pk_val);
                    }
                }
            }

            let Some(pk_val) = delta.get(pk_field).cloned().filter(|s| !s.trim().is_empty()) else {
                warn!(
                    "CDC: Skipped UPDATE on '{}' — primary key '{}' missing from binlog row pair \
                     (often MINIMAL row image + composite PK / PK detection mismatch). Table '{}'.",
                    qkey, pk_field, mirror_table
                );
                continue;
            };

            let pk_trim = pk_val.trim().to_string();
            deltas.push((pk_trim, delta));
        }
        Ok(deltas)
    }

    async fn apply_binlog_update_deltas(
        &self,
        pool: &Pool,
        schema: &str,
        mysql_table: &str,
        deltas: Vec<(String, HashMap<String, String>)>,
        qkey: &str,
        disk_entity: &str,
        mirror_table: &str,
        pk_field: &str,
        table: &mut Table,
    ) -> Result<usize> {
        let mut applied = 0usize;
        let mut hydrate_cache: HashMap<String, HashMap<String, String>> = HashMap::new();

        for (pk_trim, delta) in deltas {
            let pk_val_display = delta.get(pk_field).cloned().unwrap_or_else(|| pk_trim.clone());

            let mut lookup_candidates = vec![pk_val_display.clone(), pk_trim.clone()];
            if let Ok(n) = pk_trim.parse::<i64>() {
                lookup_candidates.push(n.to_string());
            }
            if let Ok(n) = pk_trim.parse::<u64>() {
                lookup_candidates.push(n.to_string());
            }
            lookup_candidates.sort_unstable();
            lookup_candidates.dedup();

            let mut merged = None;
            let mut storage_pk: Option<String> = None;
            for cand in &lookup_candidates {
                if cand.is_empty() {
                    continue;
                }
                if let Ok(Some(m)) = table.get_row_as_map(cand) {
                    merged = Some(m);
                    storage_pk = Some(cand.clone());
                    break;
                }
            }

            if merged.is_none() && Self::cdc_update_hydrate_enabled() {
                let fetched_opt = if let Some(cached) = hydrate_cache.get(&pk_trim) {
                    Some(cached.clone())
                } else {
                    match self
                        .fetch_full_row_mysql(pool, schema, mysql_table, qkey, pk_field, &pk_trim)
                        .await
                    {
                        Ok(Some(m)) => {
                            hydrate_cache.insert(pk_trim.clone(), m.clone());
                            debug!(
                                "CDC: Hydrated missing mirror row for UPDATE '{}' pk '{}' via SELECT",
                                qkey, pk_trim
                            );
                            Some(m)
                        }
                        Ok(None) => None,
                        Err(e) => {
                            warn!(
                                "CDC: UPDATE '{}' pk '{}' — hydrate SELECT failed ({}); skipping.",
                                qkey, pk_trim, e
                            );
                            None
                        }
                    }
                };

                if let Some(base) = fetched_opt {
                    merged = Some(base);
                    storage_pk = Some(pk_trim.clone());
                }
            }

            let mut merged = match merged {
                Some(m) => m,
                None => {
                    let detail = if Self::cdc_update_hydrate_enabled() {
                        "no mirror row and MySQL hydrate returned no row (deleted upstream, lag, or permissions)"
                    } else {
                        "no mirror row (unset BITTICE_CDC_SKIP_UPDATE_HYDRATE to pull current row from MySQL)"
                    };
                    self.log_warn(format!(
                        "CDC: UPDATE '{}' — pk '{}': {}; skipped. Table '{}'.",
                        qkey, pk_trim, detail, mirror_table
                    ));
                    continue;
                }
            };

            let storage_pk = storage_pk.unwrap_or_else(|| pk_trim.clone());

            for (k, v) in delta {
                merged.insert(k, v);
            }
            table.update(&storage_pk, merged)?;
            applied += 1;
        }
        if applied > 0 {
            let _ = self.table_manager.events_tx.send(TableUpdateEvent {
                entity: disk_entity.to_string(),
                table_name: mirror_table.to_string(),
                event_type: "UPDATE".to_string(),
                pk: String::new(),
                row: vec![applied.to_string()],
            });
        }
        Ok(applied)
    }

    fn apply_binlog_delete_rows<'a>(
        &self,
        rows: RowsEventRows<'a>,
        qkey: &str,
        disk_entity: &str,
        table_name: &str,
        pk_field: &str,
        table: &mut Table,
    ) -> Result<usize> {
        let mut applied = 0usize;
        for row_pair in rows {
            if let Ok((Some(binlog_row), _)) = row_pair {
                let row = Row::try_from(binlog_row).map_err(|e| anyhow::anyhow!("{:?}", e))?;
                let data = self.parse_row(row, qkey)?;
                if let Some(pk_val) = data.get(pk_field) {
                    table.delete(pk_val)?;
                    applied += 1;
                }
            }
        }
        if applied > 0 {
            let _ = self.table_manager.events_tx.send(TableUpdateEvent {
                entity: disk_entity.to_string(),
                table_name: table_name.to_string(),
                event_type: "DELETE".to_string(),
                pk: String::new(),
                row: vec![applied.to_string()],
            });
        }
        Ok(applied)
    }

    async fn handle_rows_event(
        &self,
        pool: &Pool,
        rows_data: RowsEventData<'_>,
        state: &mut CdcState,
        stream: &BinlogStream,
    ) -> Result<()> {
        let tm_owned: TableMapEvent<'static> = self.resolve_rows_table_map(&rows_data, stream)?;
        let schema = tm_owned.database_name().to_string();
        let table_name = tm_owned.table_name().to_string();

        if self.sync_all_databases && Self::is_system_schema(&schema) {
            return Ok(());
        }

        if self.is_out_of_scope_binlog_schema(&schema) {
            return Ok(());
        }

        if !self.is_table_in_sync_allowlist(&schema, &table_name) {
            return Ok(());
        }

        let disk_entity = if self.sync_all_databases {
            schema.to_lowercase()
        } else {
            self.entity.clone()
        };

        let qkey = {
            let maps = self.column_maps.read().unwrap();
            match Self::resolve_cdc_table_keys(self.sync_all_databases, &schema, &table_name, &maps)
            {
                Some(k) => k,
                None => {
                    self.log_warn(format!(
                        "CDC: Ignoring row event for '{}.{}' (still unknown after registration attempt — check grants or table type).",
                        schema,
                        table_name
                    ));
                    return Ok(());
                }
            }
        };

        let mirror_table = Self::resolve_mirror_table_dir(&disk_entity, &table_name);
        let table_lock = self.table_manager.get_table(&disk_entity, &mirror_table)?;
        let mut table = table_lock.write().unwrap();

        let pk_field = state
            .pk_map
            .get(&qkey)
            .cloned()
            .unwrap_or_else(|| "PK".to_string());
        table.manifest.primary_key = pk_field.clone();

        let is_write = matches!(
            &rows_data,
            RowsEventData::WriteRowsEvent(_) | RowsEventData::WriteRowsEventV1(_)
        );

        let (applied, op_label): (usize, &'static str) = match rows_data {
            RowsEventData::WriteRowsEvent(ev) => (
                self.apply_binlog_write_rows(
                    ev.rows(&tm_owned),
                    &qkey,
                    &disk_entity,
                    &mirror_table,
                    &pk_field,
                    &mut table,
                )?,
                "INSERT",
            ),
            RowsEventData::WriteRowsEventV1(ev) => (
                self.apply_binlog_write_rows(
                    ev.rows(&tm_owned),
                    &qkey,
                    &disk_entity,
                    &mirror_table,
                    &pk_field,
                    &mut table,
                )?,
                "INSERT",
            ),
            RowsEventData::UpdateRowsEvent(ev) => {
                let deltas =
                    self.extract_binlog_update_deltas(ev.rows(&tm_owned), &qkey, &mirror_table, &pk_field)?;
                (
                    self.apply_binlog_update_deltas(
                        pool,
                        &schema,
                        &table_name,
                        deltas,
                        &qkey,
                        &disk_entity,
                        &mirror_table,
                        &pk_field,
                        &mut table,
                    )
                    .await?,
                    "UPDATE",
                )
            }
            RowsEventData::UpdateRowsEventV1(ev) => {
                let deltas =
                    self.extract_binlog_update_deltas(ev.rows(&tm_owned), &qkey, &mirror_table, &pk_field)?;
                (
                    self.apply_binlog_update_deltas(
                        pool,
                        &schema,
                        &table_name,
                        deltas,
                        &qkey,
                        &disk_entity,
                        &mirror_table,
                        &pk_field,
                        &mut table,
                    )
                    .await?,
                    "UPDATE",
                )
            }
            RowsEventData::PartialUpdateRowsEvent(ev) => {
                let deltas =
                    self.extract_binlog_update_deltas(ev.rows(&tm_owned), &qkey, &mirror_table, &pk_field)?;
                (
                    self.apply_binlog_update_deltas(
                        pool,
                        &schema,
                        &table_name,
                        deltas,
                        &qkey,
                        &disk_entity,
                        &mirror_table,
                        &pk_field,
                        &mut table,
                    )
                    .await?,
                    "UPDATE",
                )
            }
            RowsEventData::DeleteRowsEvent(ev) => (
                self.apply_binlog_delete_rows(
                    ev.rows(&tm_owned),
                    &qkey,
                    &disk_entity,
                    &mirror_table,
                    &pk_field,
                    &mut table,
                )?,
                "DELETE",
            ),
            RowsEventData::DeleteRowsEventV1(ev) => (
                self.apply_binlog_delete_rows(
                    ev.rows(&tm_owned),
                    &qkey,
                    &disk_entity,
                    &mirror_table,
                    &pk_field,
                    &mut table,
                )?,
                "DELETE",
            ),
        };

        if applied > 0 {
            self.table_manager.mark_dirty(&disk_entity, &mirror_table);
            let n_flush = crate::core::cdc_durability::mirror_flush_every_n_batches();
            let max_rows_before_flush: usize = std::env::var("BITTICE_CDC_FLUSH_MAX_ROWS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(10_000);
            let do_flush = if n_flush <= 1 {
                true
            } else {
                let k = format!("{}/{}", disk_entity, mirror_table);
                let mut ticks = self.cdc_mirror_flush_ticks.write().unwrap();
                let t = ticks.entry(k).or_insert(0);
                *t = t.saturating_add(1);
                if *t >= n_flush || applied >= max_rows_before_flush {
                    *t = 0;
                    true
                } else {
                    false
                }
            };
            if do_flush {
                table.flush_active_segment()?;
            } else {
                table.flush_active_segment_buffers()?;
            }
        }
        self.maybe_log_mysql_row_batch(
            op_label,
            &schema,
            &table_name,
            &disk_entity,
            &mirror_table,
            &qkey,
            applied,
            state,
        );
        if applied > 0 {
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            state.last_mirror_batch_unix_ms = Some(ms);
            state.last_mirror_batch = Some(format!(
                "{} {}.{} qkey={} rows={} @ {}:{}",
                op_label,
                schema,
                table_name,
                qkey,
                applied,
                state.binlog_file,
                state.binlog_pos
            ));
        }
        if applied == 0 && is_write {
            self.log_warn(format!(
                "CDC: WRITE_ROWS for '{}.{}' (qkey='{}', mirror_dir='{}') applied 0 rows — \
if this persists, verify ROW-format binlog and run a current Bittice build (WRITE_ROWS uses the after-image slot).",
                schema,
                table_name,
                qkey,
                mirror_table
            ));
        }
        Ok(())
    }

    /// Maps binlog/metadata column name to the bootstrap column list (`@i` → ordinal i).
    fn binlog_column_display_name(col: &Column, bootstrap_order: &[String]) -> String {
        let s = col.name_str();
        let r = s.as_ref();
        if r.starts_with('@') {
            if let Ok(idx) = r[1..].parse::<usize>() {
                if let Some(name) = bootstrap_order.get(idx) {
                    return name.clone();
                }
            }
        }
        r.to_string()
    }

    fn parse_row(&self, row: Row, table_name: &str) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();
        let maps = self.column_maps.read().unwrap();
        let columns_names = maps.get(table_name).context("Column map not found for table")?;
        
        let d_maps = self.date_columns.read().unwrap();
        let dates = d_maps.get(table_name).cloned().unwrap_or_default();

        let e_maps = self.enum_maps.read().unwrap();
        let table_enums = e_maps.get(table_name);

        // Sparse binlog images (e.g. binlog_row_image=MINIMAL) carry fewer cells than the table
        // has columns; Row metadata maps each cell to the correct name. Full snapshots keep the
        // legacy ordinal path when lengths match (query/bootstrap paths).
        let cols_meta = row.columns_ref();
        let use_binlog_names =
            cols_meta.len() == row.len() && cols_meta.len() != columns_names.len();

        for i in 0..row.len() {
            let col_name = if use_binlog_names {
                Self::binlog_column_display_name(&cols_meta[i], columns_names)
            } else {
                columns_names.get(i).cloned().unwrap_or_else(|| format!("col_{}", i))
            };
            
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
