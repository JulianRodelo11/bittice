pub mod grpc;
pub mod table_manager;
pub mod logging;
pub mod auto_update_hint;
pub mod heartbeat;
pub mod op_counter;
pub mod self_health;
pub mod warm;
pub mod response_cache;

use axum::{
    debug_handler,
    extract::{State, Query},
    response::{IntoResponse, Json, Response},
    routing::{any},
    Router,
    http::{Method, StatusCode, HeaderMap},
    middleware::{Next},
};
use axum::extract::Request;
use std::sync::{Arc};
use std::time::Instant;
use tokio::sync::{RwLock as TokioRwLock, Notify};

/// Shared saved-ops cache for HTTP and gRPC (invalidated together on `/_config/reload`).
pub type SharedOpsCache = Arc<TokioRwLock<Option<(Instant, Arc<Vec<crate::core::saved_queries::SavedOperation>>)>>>;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use tower_http::trace::TraceLayer;
use tower_http::catch_panic::CatchPanicLayer;
use crate::core::saved_queries::{load_operations, SavedCollectAggregation, SavedOperation};
use crate::core::types::{Filter, LogicalOp, ComparisonOp, SortDirection, OrderBy, AuthContext};
use std::collections::HashMap;
use rayon::prelude::*;
use crate::core::storage::table::Table;
use crate::server::table_manager::TableManager;
use tracing::{info, debug, warn, error};

const MAX_GROUPED_RESPONSE_ROWS: usize = 10_000;

/// Saved-query API only (`/_*` rejected). Always **3000** (plus `BITTICE_HOST`).
const HTTP_QUERY_API_PORT: u16 = 3000;

/// Full REST API (`/_config`, creating/editing queries, etc.). Always **8080** (plus `BITTICE_HOST`).
const HTTP_ADMIN_API_PORT: u16 = 8080;

/// Admin listener bind override (`host:port`), e.g. private VPC IP only. If unset: `{BITTICE_HOST}:8080`.
pub fn resolve_http_internal_bind() -> String {
    if let Ok(a) = std::env::var("BITTICE_HTTP_INTERNAL_ADDR") {
        let t = a.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    format!("{}:{}", host, HTTP_ADMIN_API_PORT)
}

/// Query-only listener — fixed port **3000**.
pub fn resolve_http_public_bind() -> String {
    let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    format!("{}:{}", host, HTTP_QUERY_API_PORT)
}

/// Base URL for calling `/_config/reload` from the same machine (Repl / tooling).
/// Prefer `BITTICE_HTTP_INTERNAL_URL` when the internal bind address is not reachable via HTTP as-is (e.g. `0.0.0.0`).
pub fn http_config_reload_url() -> String {
    if let Ok(u) = std::env::var("BITTICE_HTTP_INTERNAL_URL") {
        let t = u.trim().trim_end_matches('/');
        if !t.is_empty() {
            return format!("{}/_config/reload", t);
        }
    }
    let bind = resolve_http_internal_bind();
    let host_port = bind
        .strip_prefix("0.0.0.0:")
        .map(|p| format!("127.0.0.1:{}", p))
        .unwrap_or(bind);
    format!("http://{}/_config/reload", host_port)
}

pub fn show_banner_with_filter(filter: Option<String>) {
    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[1mBittice Query Engine\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mThe engine is running and ready for requests.\x1b[0m");
    let pub_b = resolve_http_public_bind();
    println!(
        "\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m  \x1b[1mREST queries (public):\x1b[0m http://{} — saved operations only",
        pub_b
    );
    let internal_bind = resolve_http_internal_bind();
    println!(
        "\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m  \x1b[1mREST admin (private):\x1b[0m http://{} — create/edit queries, /_config, /_entities",
        internal_bind
    );
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m  \x1b[1mgRPC API:\x1b[0m    0.0.0.0:50051");

    // Show saved queries
    let ops_res = if let Some(ref f) = filter {
        crate::core::saved_queries::load_operations_with_filter(Some(f.clone()))
    } else {
        load_operations()
    };

    if let Ok(ops) = ops_res {
        if !ops.is_empty() {
            println!("\x1b[90m│\x1b[0m");
            println!("\x1b[32m◆\x1b[0m  \x1b[1mOperations available{}:\x1b[0m", 
                filter.as_ref().map(|f| format!(" for '{}'", f)).unwrap_or_default());
            for op in ops {
                println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m /{}", op.name());
            }
        }
    }

    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[1mConfig API (REST, port {} only):\x1b[0m", HTTP_ADMIN_API_PORT);
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m GET    /_config             (List all)");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m POST   /_config             (Create)");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m PUT    /_config             (Edit)");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m DELETE /_config?name=...    (Delete)");
}

pub fn show_banner() {
    show_banner_with_filter(None);
}

/// When true, CDC workers should exit their main loops (REPL back to menu or engine shutdown).
static ENGINE_HALT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Shared with [`start_all_servers`]: notified when the REPL stops the engine **or** CDC requests a fatal shutdown.
static ENGINE_SHUTDOWN_NOTIFY: Mutex<Option<Arc<Notify>>> = Mutex::new(None);

/// Set by [`request_engine_shutdown_from_cdc`] so shutdown banners distinguish user stop vs fatal binlog loss.
static ENGINE_SHUTDOWN_FROM_CDC: AtomicBool = AtomicBool::new(false);

static CDC_BACKGROUND_THREAD_HANDLES: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

#[inline]
pub fn engine_halt_requested() -> bool {
    ENGINE_HALT_REQUESTED.load(AtomicOrdering::SeqCst)
}

pub(crate) fn register_cdc_background_handle(handle: JoinHandle<()>) {
    CDC_BACKGROUND_THREAD_HANDLES.lock().unwrap().push(handle);
}

fn join_all_cdc_background_threads() {
    let handles: Vec<_> = std::mem::take(&mut *CDC_BACKGROUND_THREAD_HANDLES.lock().unwrap());
    let join_timeout = std::time::Duration::from_secs(10);
    for h in handles {
        let start = std::time::Instant::now();
        loop {
            if h.is_finished() {
                let _ = h.join();
                break;
            }
            if start.elapsed() >= join_timeout {
                warn!(
                    "CDC: Thread join timed out after {}s — orphaned worker may still be running.",
                    join_timeout.as_secs()
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

fn print_shutdown_banner_after_notify(from_cdc: bool) {
    if from_cdc {
        println!("\x1b[34m│\x1b[0m");
        println!("\x1b[33m▲\x1b[0m  \x1b[1mShutting down\x1b[0m");
        println!("\x1b[34m│\x1b[0m  \x1b[90mCDC lost the live binlog stream; stopping the engine...\x1b[0m");
        println!("\x1b[34m└\x1b[0m\n");
    } else {
        println!("\x1b[34m│\x1b[0m");
        println!("\x1b[33m▲\x1b[0m  \x1b[1mShutting down\x1b[0m");
        println!("\x1b[34m│\x1b[0m  \x1b[90mStopping Bittice engine safely...\x1b[0m");
        println!("\x1b[34m└\x1b[0m\n");
    }
}

fn finalize_shared_engine_shutdown(shutdown_notify: &Arc<Notify>) {
    ENGINE_HALT_REQUESTED.store(true, AtomicOrdering::SeqCst);
    shutdown_notify.notify_waiters();
    join_all_cdc_background_threads();
    ENGINE_HALT_REQUESTED.store(false, AtomicOrdering::SeqCst);
}

/// Full engine teardown for the interactive REPL: request CDC stop, unblock HTTP/gRPC, then join CDC threads.
///
/// **Order matters:** notify HTTP/gRPC waiters *before* joining CDC threads, so nested Tokio runtimes in
/// worker threads cannot deadlock with the main runtime while tearing down.
pub fn repl_stop_engine_and_join_cdc() {
    ENGINE_HALT_REQUESTED.store(true, AtomicOrdering::SeqCst);
    if let Some(n) = ENGINE_SHUTDOWN_NOTIFY.lock().unwrap().as_ref() {
        n.notify_waiters();
    }
    join_all_cdc_background_threads();
    ENGINE_HALT_REQUESTED.store(false, AtomicOrdering::SeqCst);
}

/// Fatal streaming failure: stop HTTP/gRPC and unblock the main `start_all_servers` wait (REPL menu or process exit).
pub fn request_engine_shutdown_from_cdc(reason: &str) {
    error!(
        "CDC: Live binlog stream failed — {}; stopping the engine.",
        reason
    );
    ENGINE_SHUTDOWN_FROM_CDC.store(true, AtomicOrdering::SeqCst);
    ENGINE_HALT_REQUESTED.store(true, AtomicOrdering::SeqCst);
    if let Some(n) = ENGINE_SHUTDOWN_NOTIFY.lock().unwrap().as_ref() {
        n.notify_waiters();
    }
}

pub(crate) async fn wait_for_exit(shutdown_notify: Arc<Notify>) -> anyhow::Result<()> {
    tokio::select! {
        res = tokio::signal::ctrl_c() => {
            res?;
            println!("\x1b[34m│\x1b[0m");
            println!("\x1b[33m▲\x1b[0m  \x1b[1mShutting down\x1b[0m");
            println!("\x1b[34m│\x1b[0m  \x1b[90mStopping Bittice engine safely...\x1b[0m");
            println!("\x1b[34m└\x1b[0m\n");
        }
        _ = shutdown_notify.notified() => {
            let from_cdc = ENGINE_SHUTDOWN_FROM_CDC.swap(false, AtomicOrdering::SeqCst);
            print_shutdown_banner_after_notify(from_cdc);
        }
    }
    finalize_shared_engine_shutdown(&shutdown_notify);
    Ok(())
}
/// When `shutdown_on_ctrl_c` is `false`, the REPL (or tests) owns shutdown via [`repl_stop_engine_and_join_cdc`]
/// with Ctrl+C on the Live Monitor; this call blocks until then.
pub async fn start_all_servers(
    entity_filter: Option<String>,
    shutdown_on_ctrl_c: bool,
) -> anyhow::Result<()> {
    let shutdown_notify = Arc::new(Notify::new());
    *ENGINE_SHUTDOWN_NOTIFY.lock().unwrap() = Some(shutdown_notify.clone());
    let table_manager = Arc::new(TableManager::new());
    let active_workers = Arc::new(StdRwLock::new(HashSet::new()));

    // Operations counter: in-memory + disk-persisted per-hour buckets.
    // Spawned before heartbeat so the first heartbeat already carries a
    // snapshot (which may be 0 / restored from disk).
    op_counter::init(&crate::core::data_paths::resolved_data_root());

    // Heartbeat + self-health POST to the Bittice control plane. Disabled when
    // control_plane_gate::ENABLED is false (local preview), even if deploy env vars are set.
    heartbeat::spawn_if_configured();

    // Self-health (consistency checks + drift diagnostics). Same gate as heartbeat.
    self_health::spawn_if_configured();

    // Convert entity_filter to lowercase and trim it
    let entity_filter = entity_filter.map(|e| e.trim().to_lowercase());

    table_manager.refresh_query_priority_keys_from_ops(entity_filter.clone());

    if let Some(ref f) = entity_filter {
        debug!("Filtering by entity: '{}'", f);
    } else {
        debug!("No entity filter applied (loading all)");
    }

    // Compact over-segmented ops tables BEFORE CDC starts — no write-lock contention.
    // Tables are in a stable on-disk state here (no live streaming yet).
    // Skip entirely if BITTICE_SKIP_STARTUP_COMPACT=1 (useful on memory-constrained hosts).
    let skip_compact = std::env::var("BITTICE_SKIP_STARTUP_COMPACT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !skip_compact {
        let tm = table_manager.clone();
        let _ = tokio::task::spawn_blocking(move || compact_startup_ops_tables(&tm)).await;
    } else {
        info!("Startup compact: skipped (BITTICE_SKIP_STARTUP_COMPACT=1).");
    }

    // --- STARTUP CONSISTENCY CHECK: validate mirror vs MySQL, repair drifted tables only ---
    if startup_consistency_check_enabled() {
        let _ = run_startup_consistency_repair(entity_filter.clone()).await;
    }

    // --- AUTO-START CDC WORKERS (sequential: HTTP only after each profile signals Phase 4) ---
    if cdc_autostart_enabled() {
        let specs = collect_cdc_spawn_specs(&entity_filter, &active_workers);
        if specs.is_empty() {
            let n = crate::core::data_paths::cdc_profile_count(
                &crate::core::data_paths::resolved_data_root(),
            );
            if n == 0 {
                warn!(
                    "CDC autostart: no profiles/*/cdc_config.json under {} — serving static mirror data only. \
                     Sync on your PC (Connect and sync) or redeploy data/ to this host.",
                    crate::core::data_paths::resolved_data_root().display()
                );
            }
        } else {
            info!(
                "CDC: Staged startup — {} profile(s) run one after another; ports 3000/8080 open only after each finishes Phase 4.",
                specs.len()
            );
            let tm = table_manager.clone();
            let aw = active_workers.clone();
            let staged = tokio::task::spawn_blocking(move || {
                run_cdc_staged_sequential(tm, aw, specs)
            })
            .await
            .map_err(|e| anyhow::anyhow!("staged CDC join failed: {}", e))?;
            if !staged.deferred.is_empty() {
                warn!(
                    "CDC: {} profile(s) deferred — HTTP/gRPC will serve static mirror until background CDC reconnects.",
                    staged.deferred.len()
                );
                spawn_deferred_cdc_retries(
                    staged.deferred,
                    table_manager.clone(),
                    active_workers.clone(),
                );
            }
        }
    } else {
        info!("CDC autostart disabled. Running with static local data only.");
    }

    crate::server::auto_update_hint::spawn_if_configured();

    let shared_ops_cache: SharedOpsCache = Arc::new(TokioRwLock::new(None));

    let http_tm = table_manager.clone();
    let http_filter = entity_filter.clone();
    let http_active = active_workers.clone();
    let sn_http = shutdown_notify.clone();
    let http_ops_cache = shared_ops_cache.clone();
    tokio::spawn(async move {
        start_server(http_tm, http_filter, http_active, sn_http, http_ops_cache).await;
    });

    let grpc_tm = table_manager.clone();
    let grpc_filter = entity_filter.clone();
    let sn_grpc = shutdown_notify.clone();
    tokio::spawn(async move {
        let _ = grpc::start_grpc_server_with_manager(
            50051,
            grpc_tm,
            grpc_filter,
            None,
            sn_grpc,
            shared_ops_cache,
        )
        .await;
    });

    if shutdown_on_ctrl_c {
        show_banner();
        wait_for_exit(shutdown_notify).await
    } else {
        shutdown_notify.notified().await;
        let from_cdc = ENGINE_SHUTDOWN_FROM_CDC.swap(false, AtomicOrdering::SeqCst);
        if from_cdc {
            print_shutdown_banner_after_notify(true);
            finalize_shared_engine_shutdown(&shutdown_notify);
        }
        Ok(())
    }
}

use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::RwLock as StdRwLock;

#[derive(Clone)]
struct CdcSpawnSpec {
    entity: String,
    entity_key: String,
    worker_db: String,
    url: String,
    sync_all: bool,
    cleanup_single_db_lock: Option<String>,
    db_name_for_log: String,
    infinite_connect_retries: bool,
    connect_attempt_limit: Option<u32>,
}

struct StagedCdcStartupResult {
    deferred: Vec<CdcSpawnSpec>,
}

/// Build launch plans for every CDC profile that should auto-start (same rules as historical `scan_and_start_cdc`).
fn collect_cdc_spawn_specs(
    entity_filter: &Option<String>,
    active_workers: &Arc<StdRwLock<HashSet<String>>>,
) -> Vec<CdcSpawnSpec> {
    const SINGLE_DB_WORKER_LOCK_PREFIX: &str = "__single_db_worker_lock__";
    let mut out = Vec::new();

    for config_path in crate::core::data_paths::scan_all_cdc_config_paths() {
        let entity_folder_name = config_path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if entity_folder_name.is_empty() {
            continue;
        }

        if let Some(ref filter) = entity_filter {
            if entity_folder_name.to_lowercase() != *filter {
                continue;
            }
        }

        let Ok(content) = std::fs::read_to_string(&config_path) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };

        let user = config["user"].as_str().unwrap_or_default().to_string();
        let pass = config["pass"].as_str().unwrap_or_default().to_string();
        let mut host = config["host"].as_str().unwrap_or_default().to_string();
        let sync_all = config["sync_all_databases"].as_bool().unwrap_or(false);
        let db = config["database"].as_str().unwrap_or_default().to_string();
        let entity = config["entity"]
            .as_str()
            .unwrap_or(&entity_folder_name)
            .to_string();

        let entity_key = entity.clone();
        {
            let active = active_workers.read().unwrap();
            if active.contains(&entity_key) {
                continue;
            }
        }

        if let Some(ref filter) = entity_filter {
            if entity.to_lowercase() != *filter {
                continue;
            }
        }

        let port = if let Some(p) = config["port"].as_str() {
            p.to_string()
        } else if let Some(p) = config["port"].as_u64() {
            p.to_string()
        } else {
            "3306".to_string()
        };

        let is_docker =
            std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
        if (host == "localhost" || host == "0.0.0.0") && is_docker {
            host = "host.docker.internal".to_string();
        }

        let single_db_lock_key = format!(
            "{}:{}:{}",
            SINGLE_DB_WORKER_LOCK_PREFIX,
            host.to_lowercase(),
            port
        );

        if !sync_all {
            let active = active_workers.read().unwrap();
            if active.contains(&single_db_lock_key) {
                warn!(
                    "CDC: Skipping entity '{}' — another profile already owns single-database CDC for {}:{}. \
MySQL publishes one binlog per server; Bittice allows only one single-DB worker per host:port. \
Mirror '{}' will stay static unless you enable sync_all_databases on one profile for this server (covers all schemas in one stream) or use separate MySQL instances.",
                    entity,
                    host,
                    port,
                    entity,
                );
                continue;
            }
        }

        let url = if sync_all {
            format!("mysql://{}:{}@{}:{}/", user, pass, host, port)
        } else {
            format!("mysql://{}:{}@{}:{}/{}", user, pass, host, port, db)
        };
        let worker_db = if sync_all {
            String::new()
        } else {
            db.clone()
        };
        let db_name_for_log = if sync_all {
            entity.clone()
        } else {
            worker_db.clone()
        };

        out.push(CdcSpawnSpec {
            entity,
            entity_key,
            worker_db,
            url,
            sync_all,
            cleanup_single_db_lock: if sync_all {
                None
            } else {
                Some(single_db_lock_key)
            },
            db_name_for_log,
            infinite_connect_retries: false,
            connect_attempt_limit: None,
        });
    }

    out
}

fn spawn_cdc_worker_thread(
    spec: CdcSpawnSpec,
    table_manager: Arc<TableManager>,
    active_workers: Arc<StdRwLock<HashSet<String>>>,
    startup_report_tx: Option<mpsc::Sender<crate::core::cdc::CdcStartupReport>>,
) {
    let reached_live = Arc::new(AtomicBool::new(false));
    spawn_cdc_worker_thread_inner(
        spec,
        table_manager,
        active_workers,
        startup_report_tx,
        Some(reached_live.clone()),
    );
}

fn spawn_cdc_worker_thread_inner(
    spec: CdcSpawnSpec,
    table_manager: Arc<TableManager>,
    active_workers: Arc<StdRwLock<HashSet<String>>>,
    startup_report_tx: Option<mpsc::Sender<crate::core::cdc::CdcStartupReport>>,
    reached_live: Option<Arc<AtomicBool>>,
) {

    let cleanup_entity_key = spec.entity_key.clone();
    let cleanup_single_db_lock = spec.cleanup_single_db_lock.clone();
    let cleanup_active_workers = active_workers.clone();
    let infinite_connect_retries = spec.infinite_connect_retries;
    let connect_attempt_limit = spec.connect_attempt_limit;

    {
        let mut active = active_workers.write().unwrap();
        active.insert(spec.entity_key.clone());
        if let Some(ref lock_key) = spec.cleanup_single_db_lock {
            active.insert(lock_key.clone());
        }
    }

    let db_name_for_log = spec.db_name_for_log.clone();

    let h = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let worker = crate::core::cdc::CdcWorker::with_manager_and_log_ex(
            spec.url,
            spec.entity,
            spec.worker_db,
            table_manager,
            None,
            spec.sync_all,
            false,
            startup_report_tx,
            reached_live.clone(),
            infinite_connect_retries,
            connect_attempt_limit,
        );
        if let Err(e) = rt.block_on(worker.run()) {
            error!("CDC: Worker for '{}' failed: {:#}", db_name_for_log, e);
            let shutdown = reached_live
                .as_ref()
                .map(|l| l.load(AtomicOrdering::Acquire))
                .unwrap_or(true);
            if shutdown {
                crate::server::request_engine_shutdown_from_cdc(&format!(
                    "CDC worker '{}' failed: {:#}",
                    db_name_for_log, e
                ));
            } else {
                warn!(
                    "CDC: Worker for '{}' failed before live replication; engine continues on static mirror.",
                    db_name_for_log
                );
            }
        }
        let mut active = cleanup_active_workers.write().unwrap();
        active.remove(&cleanup_entity_key);
        if let Some(lock_key) = cleanup_single_db_lock {
            active.remove(&lock_key);
        }
    });
    register_cdc_background_handle(h);
}

/// Compact any ops tables that have accumulated excessive micro-segments.
/// Called once at startup (blocking), after CDC workers reach Phase 4, before HTTP opens.
fn compact_startup_ops_tables(table_manager: &TableManager) {
    let keys = match table_manager.get_query_priority_keys() {
        Some(k) if !k.is_empty() => k,
        _ => return,
    };
    println!(
        "\x1b[90m│\x1b[0m  \x1b[34m→\x1b[0m  Checking {} op table(s) for compaction…",
        keys.len()
    );
    info!("Startup compact: checking {} ops table(s) for segment fragmentation…", keys.len());
    let mut compacted = 0usize;
    for key in keys.iter() {
        let mut parts = key.splitn(2, '/');
        let (entity, table_name) = match (parts.next(), parts.next()) {
            (Some(e), Some(t)) => (e, t),
            _ => continue,
        };
        let table_arc = match table_manager.get_table(entity, table_name) {
            Ok(t) => t,
            Err(e) => {
                warn!("Startup compact: cannot open {}: {:#}", key, e);
                continue;
            }
        };
        let result = table_arc.write().map(|mut t| t.compact());
        match result {
            Ok(Ok(0)) => {}
            Ok(Ok(reduced)) => {
                info!("Startup compact: {} — reduced by {} segment(s)", key, reduced);
                compacted += 1;
            }
            Ok(Err(e)) => warn!("Startup compact: {} error: {:#}", key, e),
            Err(e) => warn!("Startup compact: {} lock error: {}", key, e),
        }
        table_manager.close_table(entity, table_name);
    }
    if compacted > 0 {
        info!("Startup compact: finished — {} table(s) compacted.", compacted);
    } else {
        info!("Startup compact: all ops tables are already well-segmented.");
    }
}

fn run_cdc_staged_sequential(
    table_manager: Arc<TableManager>,
    active_workers: Arc<StdRwLock<HashSet<String>>>,
    specs: Vec<CdcSpawnSpec>,
) -> StagedCdcStartupResult {
    use crate::core::cdc::CdcStartupOutcome;

    let mut deferred = Vec::new();
    let total = specs.len();
    for (idx, spec) in specs.into_iter().enumerate() {
        info!(
            "CDC: ─── Staged profile {}/{}: '{}' ───",
            idx + 1,
            total,
            spec.entity
        );
        let data_root = crate::core::data_paths::resolved_data_root();
        let root_disp = std::fs::canonicalize(&data_root).unwrap_or_else(|_| data_root.clone());
        let state_pb = crate::core::data_paths::profile_dir(&spec.entity).join("cdc_state.json");
        let state_disp = std::fs::canonicalize(&state_pb).unwrap_or_else(|_| state_pb.clone());
        info!(
            "CDC: [{}] sync_all={} data_root={} cdc_state_path={}",
            spec.entity,
            spec.sync_all,
            root_disp.display(),
            state_disp.display()
        );
        println!(
            "\x1b[90m│\x1b[0m  \x1b[34m→\x1b[0m  [{}/{}] '{}': connecting and syncing…",
            idx + 1,
            total,
            spec.entity
        );
        let (tx, rx) = mpsc::channel();
        spawn_cdc_worker_thread(spec.clone(), table_manager.clone(), active_workers.clone(), Some(tx));
        match rx.recv() {
            Ok(report) => match report.outcome {
                CdcStartupOutcome::LiveReplication => {
                    info!(
                        "CDC: Profile '{}' finished Phase 4 — live replication running.",
                        report.entity
                    );
                    println!(
                        "\x1b[90m│\x1b[0m  \x1b[32m✓\x1b[0m  '{}': live replication active.",
                        report.entity
                    );
                }
                CdcStartupOutcome::StaticMirror => {
                    warn!(
                        "CDC: Profile '{}' could not reach live replication; deferring and serving static mirror.",
                        report.entity
                    );
                    println!(
                        "\x1b[90m│\x1b[0m  \x1b[33m▲\x1b[0m  '{}': static mirror — background CDC retry scheduled.",
                        report.entity
                    );
                    deferred.push(spec);
                }
                CdcStartupOutcome::Failed(msg) => {
                    warn!(
                        "CDC: Profile '{}' failed during startup ({}); deferring and serving static mirror.",
                        report.entity, msg
                    );
                    println!(
                        "\x1b[90m│\x1b[0m  \x1b[33m▲\x1b[0m  '{}': startup failed — background CDC retry scheduled.",
                        report.entity
                    );
                    deferred.push(spec);
                }
            },
            Err(_) => {
                warn!(
                    "CDC: Worker for staged profile '{}' exited without a readiness report; deferring.",
                    spec.entity
                );
                deferred.push(spec);
            }
        }
    }
    StagedCdcStartupResult { deferred }
}

fn spawn_deferred_cdc_retries(
    deferred: Vec<CdcSpawnSpec>,
    table_manager: Arc<TableManager>,
    active_workers: Arc<StdRwLock<HashSet<String>>>,
) {
    for mut spec in deferred {
        spec.infinite_connect_retries = true;
        spec.connect_attempt_limit = None;
        let tm = table_manager.clone();
        let aw = active_workers.clone();
        let entity = spec.entity.clone();
        std::thread::spawn(move || {
            let mut idle_secs = 60u64;
            loop {
                info!(
                    "CDC: background retry for deferred profile '{}' (next attempt in {}s)…",
                    entity, idle_secs
                );
                std::thread::sleep(std::time::Duration::from_secs(idle_secs));
                let (tx, rx) = mpsc::channel();
                spawn_cdc_worker_thread(spec.clone(), tm.clone(), aw.clone(), Some(tx));
                match rx.recv() {
                    Ok(report) if matches!(report.outcome, crate::core::cdc::CdcStartupOutcome::LiveReplication) => {
                        info!(
                            "CDC: deferred profile '{}' is now live after background retry.",
                            entity
                        );
                        break;
                    }
                    Ok(report) => {
                        warn!(
                            "CDC: deferred profile '{}' background attempt ended with {:?}; will retry.",
                            entity, report.outcome
                        );
                    }
                    Err(_) => {
                        warn!(
                            "CDC: deferred profile '{}' worker exited without report; will retry.",
                            entity
                        );
                    }
                }
                idle_secs = (idle_secs.saturating_mul(2)).min(600);
            }
        });
    }
}

pub struct ServerState {
    pub table_manager: Arc<TableManager>,
    pub ops_cache: Arc<TokioRwLock<Option<(Instant, Arc<Vec<SavedOperation>>)>>>,
    pub response_cache: Arc<response_cache::ResponseCache>,
    pub entity_filter: Option<String>,
    pub auth_service: crate::core::auth::AuthService,
    pub active_workers: Arc<StdRwLock<HashSet<String>>>,
}

pub fn scan_and_start_cdc(
    table_manager: Arc<TableManager>,
    entity_filter: Option<String>,
    active_workers: Arc<StdRwLock<HashSet<String>>>,
) {
    let specs = collect_cdc_spawn_specs(&entity_filter, &active_workers);
    for spec in specs {
        spawn_cdc_worker_thread(spec, table_manager.clone(), active_workers.clone(), None);
    }
}

fn startup_consistency_check_enabled() -> bool {
    std::env::var("BITTICE_STARTUP_CONSISTENCY_CHECK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Runs `check-mirror` against every bootstrapped table before CDC opens HTTP.
/// Tables with non-trivial drift have their bootstrap state invalidated so CDC
/// re-bootstraps them from MySQL (non-aggressive — only drifted tables are repaired).
async fn run_startup_consistency_repair(entity_filter: Option<String>) -> anyhow::Result<()> {
    use crate::core::mirror_consistency;

    let opts = mirror_consistency::CheckMirrorOptions {
        entity_filter,
        table_filter: None,
        revalidate: true,
    };

    let rows = match mirror_consistency::check_mirror_consistency(opts).await {
        Ok(r) => r,
        Err(e) => {
            warn!("Startup consistency check: mirror_consistency failed ({:#}); skipping repair, CDC will start normally.", e);
            return Ok(());
        }
    };

    if rows.is_empty() {
        info!("Startup consistency check: no bootstrapped tables — skipping.");
        return Ok(());
    }

    // Only repair tables where drift exceeds a reasonable threshold.
    // Small diffs (≤10 rows) can be timing artifacts between MySQL COUNT and mirror read.
    let drifted: Vec<_> = rows
        .iter()
        .filter(|r| !r.ok && (r.diff.abs() > 10))
        .collect();

    if drifted.is_empty() {
        let tiny = rows.iter().filter(|r| !r.ok).count();
        if tiny > 0 {
            info!(
                "Startup consistency check: {} table(s) with trivial drift (≤10 rows) — skipping repair.",
                tiny
            );
        }
        info!(
            "Startup consistency check: all {} table(s) match MySQL.",
            rows.len()
        );
        return Ok(());
    }

    warn!(
        "Startup consistency check: {} of {} table(s) have drift — repairing...",
        drifted.len(),
        rows.len()
    );
    for row in &drifted {
        info!(
            "  DRIFT {}: mysql={} mirror={} diff={}",
            row.table, row.source_count, row.mirror_count, row.diff
        );
    }

    let data_root = crate::core::data_paths::resolved_data_root();

    // Group drifted tables by profile
    let mut by_profile: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for row in &drifted {
        by_profile
            .entry(row.profile.clone())
            .or_default()
            .push(row.table.clone());
    }

    for (profile, tables) in &by_profile {
        let state_path = crate::core::data_paths::profile_dir(profile).join("cdc_state.json");
        let cfg_path = crate::core::data_paths::profile_dir(profile).join("cdc_config.json");

        let Ok(cfg_raw) = std::fs::read_to_string(&cfg_path) else {
            warn!("  skip {}: cannot read cdc_config.json", profile);
            continue;
        };
        let Ok(cfg_json): Result<serde_json::Value, _> =
            serde_json::from_str(&cfg_raw)
        else {
            warn!("  skip {}: invalid cdc_config.json", profile);
            continue;
        };
        let Ok(state_raw) = std::fs::read_to_string(&state_path) else {
            warn!("  skip {}: cannot read cdc_state.json", profile);
            continue;
        };
        let Ok(mut state_json): Result<serde_json::Value, _> =
            serde_json::from_str(&state_raw)
        else {
            warn!("  skip {}: invalid cdc_state.json", profile);
            continue;
        };

        let sync_all = cfg_json
            .get("sync_all_databases")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let database = cfg_json
            .get("database")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let entity = cfg_json
            .get("entity")
            .and_then(|v| v.as_str())
            .unwrap_or(profile);

        let mut removed = 0usize;

        if let Some(bootstrapped) = state_json.get_mut("bootstrapped_tables") {
            if let Some(arr) = bootstrapped.as_array_mut() {
                let before = arr.len();
                arr.retain(|v| {
                    let qkey = v.as_str().unwrap_or("");
                    !tables.iter().any(|t| t == qkey)
                });
                removed = before - arr.len();
            }
        }

        if let Some(pk_map) = state_json.get_mut("pk_map") {
            if let Some(obj) = pk_map.as_object_mut() {
                for table in tables {
                    obj.remove(table);
                }
            }
        }

        if let Ok(json_str) = serde_json::to_string_pretty(&state_json) {
            if let Err(e) = std::fs::write(&state_path, &json_str) {
                error!(
                    "Startup consistency repair: write {} failed: {}",
                    state_path.display(),
                    e
                );
            }
        }

        for qkey in tables {
            let (schema, table) =
                mirror_consistency::parse_qkey(sync_all, database, qkey);
            let disk_entity = if sync_all {
                schema.to_lowercase()
            } else {
                entity.to_string()
            };
            let mirror_dir = mirror_consistency::resolve_mirror_dir(
                &data_root, &disk_entity, &table,
            );

            if mirror_dir.exists() {
                info!(
                    "  Remove mirror dir: {}",
                    mirror_dir.display()
                );
                if let Err(e) = std::fs::remove_dir_all(&mirror_dir) {
                    error!(
                        "  Failed to remove {}: {}",
                        mirror_dir.display(),
                        e
                    );
                }
            }
        }

        info!(
            "  {}: invalidated {}/{} drifted table(s) — CDC will re-bootstrap them.",
            profile,
            removed,
            drifted.len()
        );
    }

    warn!(
        "Startup consistency repair complete: {} table(s) invalidated. CDC will re-bootstrap from MySQL.",
        drifted.len()
    );

    Ok(())
}

fn cdc_autostart_enabled() -> bool {
    std::env::var("BITTICE_DISABLE_CDC_AUTOSTART")
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            !(normalized == "1" || normalized == "true" || normalized == "yes" || normalized == "on")
        })
        .unwrap_or(true)
}

pub async fn start_server(
    table_manager: Arc<TableManager>, 
    entity_filter: Option<String>, 
    active_workers: Arc<StdRwLock<HashSet<String>>>,
    shutdown_notify: Arc<Notify>,
    ops_cache: SharedOpsCache,
) {
    let state = Arc::new(ServerState {
        table_manager: table_manager.clone(),
        ops_cache,
        response_cache: Arc::new(response_cache::ResponseCache::from_env()),
        entity_filter: entity_filter.clone(),
        auth_service: crate::core::auth::AuthService::new(table_manager),
        active_workers,
    });
    if state.response_cache.enabled() {
        info!(
            "Response cache enabled (TTL={}s, ops={})",
            std::env::var("BITTICE_RESPONSE_CACHE_TTL_SECS").unwrap_or_default(),
            std::env::var("BITTICE_RESPONSE_CACHE_OPS").unwrap_or_default()
        );
    }

    // Authentication middleware (applied per-router; state is Arc-cloned)
    let app_internal = Router::new()
        .route("/*path", any(handle_request))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(state.clone());

    let internal_bind = resolve_http_internal_bind();
    let listener_internal = match tokio::net::TcpListener::bind(&internal_bind).await {
        Ok(l) => l,
        Err(e) => {
            error!(
                "Could not bind internal HTTP server to {}: {}",
                internal_bind, e
            );
            return;
        }
    };
    info!(
        "HTTP admin API (full, incl. /_*) listening on http://{}",
        internal_bind
    );

    // Warm cache in background (P0 filters first; P1 skips huge tables).
    warm::spawn_background_warm(state.clone());

    let public_bind = resolve_http_public_bind();

    let app_public = Router::new()
        .route("/*path", any(handle_public_request))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Op counter wraps auth+handler so it sees the *final* status:
        // 401 from auth, 4xx/5xx from handler, 2xx success. Only success
        // bumps the bill. Lives outside auth so admin endpoints (none on
        // this router, but defensive) wouldn't be affected anyway.
        .layer(axum::middleware::from_fn(count_billable_request))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(state.clone());

    let listener_public = match tokio::net::TcpListener::bind(&public_bind).await {
        Ok(l) => l,
        Err(e) => {
            error!(
                "Could not bind public query HTTP server to {}: {}",
                public_bind, e
            );
            return;
        }
    };
    info!(
        "HTTP query API (saved operations only; /_* → 404) on http://{}",
        public_bind
    );

    let _ = tokio::join!(
        axum::serve(listener_internal, app_internal).with_graceful_shutdown({
            let n = shutdown_notify.clone();
            async move {
                n.notified().await;
            }
        }),
        axum::serve(listener_public, app_public).with_graceful_shutdown({
            let n = shutdown_notify.clone();
            async move {
                n.notified().await;
            }
        }),
    );
}

pub async fn auth_middleware(
    _state: State<Arc<ServerState>>,
    _headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Middleware is a no-op here: identity resolution runs on demand in `handle_request`
    // using each saved operation's auth configuration.
    Ok(next.run(request).await)
}

/// Wraps the public router. Bumps the op counter exactly once for any
/// 2xx response (one unary op). 4xx/5xx never bump — failed queries are
/// free. gRPC and streaming notifications are counted separately in
/// `grpc.rs`.
pub async fn count_billable_request(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if response.status().is_success() {
        op_counter::bump(op_counter::OpType::Unary);
    }
    response
}

#[debug_handler]
async fn handle_public_request(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    method: Method,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let path = uri.path();
    if path.starts_with("/_") {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "detail": format!(
                    "Use the admin HTTP port ({}) for paths starting with /_ (e.g. POST /_config to create queries).",
                    HTTP_ADMIN_API_PORT
                ),
            })),
        )
            .into_response();
    }
    handle_request(State(state), headers, extensions, method, uri, body)
        .await
        .into_response()
}

#[debug_handler]
async fn handle_request(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    method: Method,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let auth_context = extensions.get::<AuthContext>();
    let start_total = std::time::Instant::now();
    let path = uri.path().to_string();
    let query_params: HashMap<String, String> = Query::try_from_uri(&uri)
        .map(|Query(params)| params)
        .unwrap_or_default();
    
    let op_name = path.trim_start_matches('/').to_string();
    
    let raw_auth_token = crate::core::auth::extract_credential_from_headers(&headers);

    // Non-blocking log send to avoid hanging the request
    info!("{} /{}", method, op_name);
    
    // Load operations with improved caching and filtering
    let ops: Arc<Vec<SavedOperation>> = {
        let cache_read = state.ops_cache.read().await;
        let needs_reload = match &*cache_read {
            Some((ts, _)) => ts.elapsed().as_secs() > 60,
            None => true,
        };
        if needs_reload {
            drop(cache_read);
            let mut cache_write = state.ops_cache.write().await;
            // Check again after acquiring write lock
            if let Some((ts, cached_ops)) = &*cache_write {
                if ts.elapsed().as_secs() <= 60 {
                    cached_ops.clone()
                } else {
                    let loaded = crate::core::saved_queries::load_operations_with_filter(state.entity_filter.clone()).unwrap_or_default();
                    let loaded_arc = Arc::new(loaded);
                    *cache_write = Some((std::time::Instant::now(), loaded_arc.clone()));
                    loaded_arc
                }
            } else {
                let loaded = crate::core::saved_queries::load_operations_with_filter(state.entity_filter.clone()).unwrap_or_default();
                let loaded_arc = Arc::new(loaded);
                *cache_write = Some((std::time::Instant::now(), loaded_arc.clone()));
                loaded_arc
            }
        } else {
            cache_read.as_ref().unwrap().1.clone()
        }
    };
    let ops_load_ms = start_total.elapsed().as_secs_f64() * 1000.0;

    if path == "/_entities" {
        let mut catalog = serde_json::Map::new();
        for entity_path in crate::core::data_paths::iter_mirror_entity_paths() {
            let entity_name = entity_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if entity_name.is_empty() {
                continue;
            }
            let mut tables = Vec::new();
            if let Ok(table_entries) = std::fs::read_dir(&entity_path) {
                for table_entry in table_entries.flatten() {
                    if table_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        tables.push(table_entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
            catalog.insert(entity_name, serde_json::json!(tables));
        }
        return (StatusCode::OK, Json(serde_json::Value::Object(catalog))).into_response();
    }

    if path == "/_config/reload" {
        info!("Hot-reloading configuration from disk...");
        {
            let mut cache = state.ops_cache.write().await;
            *cache = None;
        }
        state.response_cache.clear();
        state
            .table_manager
            .refresh_query_priority_keys_from_ops(state.entity_filter.clone());
        if cdc_autostart_enabled() {
            scan_and_start_cdc(state.table_manager.clone(), state.entity_filter.clone(), state.active_workers.clone());
        } else {
            info!("CDC autostart is disabled; skipping CDC worker scan on reload.");
        }
        return (StatusCode::OK, Json(serde_json::json!({ "status": "success", "message": "Configuration reloaded" }))).into_response();
    }

    if path == "/_config" {
        match method {
            Method::POST => {
                match serde_json::from_slice::<SavedOperation>(&body) {
                    Ok(new_op) => {
                        let name = new_op.name().to_string();
                        let mut all_ops = crate::core::saved_queries::load_operations().unwrap_or_default();
                        if all_ops.iter().any(|o| o.name() == name) {
                            (StatusCode::CONFLICT, Json(serde_json::json!({ "error": format!("Operation '{}' already exists. Use PUT to update.", name) }))).into_response()
                        } else {
                            all_ops.push(new_op);
                            if let Err(e) = crate::core::saved_queries::save_operations(&all_ops) {
                                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Failed to save configuration", "details": e.to_string() }))).into_response();
                            }
                            { let mut cache = state.ops_cache.write().await; *cache = None; }
                            state.table_manager.refresh_query_priority_keys_from_ops(
                                state.entity_filter.clone(),
                            );
                            (StatusCode::CREATED, Json(serde_json::json!({ "status": "created", "name": name }))).into_response()
                        }
                    },
                    Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Invalid format", "details": e.to_string() }))).into_response()
                }
            },
            Method::PUT => {
                match serde_json::from_slice::<SavedOperation>(&body) {
                    Ok(new_op) => {
                        let name = new_op.name().to_string();
                        let mut all_ops = crate::core::saved_queries::load_operations().unwrap_or_default();
                        if let Some(pos) = all_ops.iter().position(|o| o.name() == name) {
                            all_ops[pos] = new_op;
                            if let Err(e) = crate::core::saved_queries::save_operations(&all_ops) {
                                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Failed to save configuration", "details": e.to_string() }))).into_response();
                            }
                            { let mut cache = state.ops_cache.write().await; *cache = None; }
                            state.table_manager.refresh_query_priority_keys_from_ops(
                                state.entity_filter.clone(),
                            );
                            (StatusCode::OK, Json(serde_json::json!({ "status": "updated", "name": name }))).into_response()
                        } else {
                            (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("Operation '{}' not found. Use POST to create.", name) }))).into_response()
                        }
                    },
                    Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Invalid format", "details": e.to_string() }))).into_response()
                }
            },
            Method::GET => {
                if let Some(name) = query_params.get("name") {
                    if let Some(op) = ops.iter().find(|o: &&SavedOperation| o.name() == name) {
                        (StatusCode::OK, Json(serde_json::json!(op))).into_response()
                    } else {
                        (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Operation not found" }))).into_response()
                    }
                } else {
                    (StatusCode::OK, Json(serde_json::json!(&*ops))).into_response()
                }
            },
            Method::DELETE => {
                if let Some(name_to_del) = query_params.get("name") {
                    let mut all_ops = crate::core::saved_queries::load_operations().unwrap_or_default();
                    let initial_len = all_ops.len();
                    all_ops.retain(|o| o.name() != name_to_del);
                    if all_ops.len() < initial_len {
                        if let Err(e) = crate::core::saved_queries::save_operations(&all_ops) {
                            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Failed to save configuration", "details": e.to_string() }))).into_response();
                        }
                        { let mut cache = state.ops_cache.write().await; *cache = None; }
                        state.table_manager.refresh_query_priority_keys_from_ops(
                            state.entity_filter.clone(),
                        );
                        (StatusCode::OK, Json(serde_json::json!({ "status": "deleted", "name": name_to_del }))).into_response()
                    } else {
                        (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Operation not found" }))).into_response()
                    }
                } else {
                    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Missing 'name' query parameter" }))).into_response()
                }
            },
            _ => (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({ "error": "Method not allowed for /_config" }))).into_response()
        }
    } else {
        // Custom operations
        let operation = ops.iter().find(|o: &&SavedOperation| o.name() == op_name);
        if let Some(op) = operation {
            // Check for custom AuthConfig in the operation
            let mut effective_auth_ctx = auth_context.cloned();
            if let SavedOperation::Read(ref q) = op {
                if let Some(auth_cfg) = &q.auth_config {
                    if auth_cfg.enabled {
                        if let Some(token) = &raw_auth_token {
                            debug!("Using custom AuthConfig for operation '{}' (table: {})", op_name, auth_cfg.table);
                            effective_auth_ctx = state.auth_service.resolve_token(&q.entity, token, Some(auth_cfg)).await;

                            
                            // Strict: token could not be resolved (invalid token or unknown user)
                            if effective_auth_ctx.is_none() {
                                warn!("-> 401 Unauthorized (Identity resolution failed for {})", op_name);
                                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ 
                                    "error": "Unauthorized", 
                                    "details": "Identity could not be resolved with the provided token." 
                                }))).into_response();
                            }
                        } else {
                            // Strict: bearer token is missing
                            warn!("-> 401 Unauthorized (No token provided for {})", op_name);
                            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ 
                                    "error": "Unauthorized", 
                                    "details": "Authorization: Bearer <api_key> or X-API-Key header is required for this operation." 
                                }))).into_response();
                        }
                    }
                }
            }

            match (method, op) {
                (Method::GET, SavedOperation::Read(ref q)) => {
                    if effective_auth_ctx.is_none()
                        && state.response_cache.is_cacheable_op(&op_name)
                    {
                        if let Some(cached) = state.response_cache.get(&op_name, &query_params) {
                            return (StatusCode::OK, Json(cached)).into_response();
                        }
                    }
                    let read_result = if let Some(crate::core::saved_queries::SavedExecutionProfile::Split(profile)) = &q.execution_profile {
                        execute_split_enrichment_read(
                            q,
                            profile,
                            query_params.clone(),
                            state.clone(),
                            start_total,
                            ops_load_ms,
                            effective_auth_ctx.as_ref(),
                        )
                        .await
                    } else {
                        execute_read_operation(q, query_params.clone(), state.clone(), start_total, ops_load_ms, effective_auth_ctx.as_ref()).await
                    };
                    match read_result {
                        Ok(val) => {
                            if effective_auth_ctx.is_none()
                                && state.response_cache.is_cacheable_op(&op_name)
                            {
                                state.response_cache.put(&op_name, &query_params, val.clone());
                            }
                            (StatusCode::OK, Json(val)).into_response()
                        }
                        Err((status, val)) => (status, Json(val)).into_response(),
                    }
                },
                (Method::GET, SavedOperation::Batch(ref b)) => {
                    if state.response_cache.is_cacheable_op(&op_name) {
                        if let Some(cached) = state.response_cache.get(&op_name, &query_params) {
                            return (StatusCode::OK, Json(cached)).into_response();
                        }
                    }
                    handle_batch(b, query_params, state, Some(op_name.as_str())).await.into_response()
                },
                (Method::POST, SavedOperation::Insert(ref i)) => {
                    let payload: HashMap<String, String> = serde_json::from_slice(&body).unwrap_or_default();
                    handle_insert(i, payload, state).await.into_response()
                },
                (m, _) => {
                    warn!("-> 405 Method Not Allowed ({})", m);
                    (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({ "error": "Method not allowed for this operation" }))).into_response()
                }
            }
        } else {
            warn!("-> 404 Not Found ('{}')", op_name);
            (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("Operation '{}' not found", op_name) }))).into_response()
        }
    }
}

async fn handle_batch(
    batch: &crate::core::saved_queries::SavedBatch,
    params: HashMap<String, String>,
    state: Arc<ServerState>,
    cache_op_name: Option<&str>,
) -> impl IntoResponse {
    let mut results = serde_json::Map::new();
    let ops = {
        let mut cache = state.ops_cache.write().await;
        let loaded = crate::core::saved_queries::load_operations_with_filter(state.entity_filter.clone()).unwrap_or_default();
        let loaded_arc = Arc::new(loaded);
        *cache = Some((std::time::Instant::now(), loaded_arc.clone()));
        loaded_arc
    };
    
    let mut max_pages = 0;
    let mut total_items_sum = 0;
    let mut execution_time_sum = 0.0;

    for op_name in &batch.operations {
        if let Some(op) = ops.iter().find(|o: &&SavedOperation| o.name() == op_name) {
            match op {
                SavedOperation::Read(ref q) => {
                    let mut targeted_params = params.clone();
                    let prefix = format!("{}:", op_name);
                    for (k, v) in &params {
                        if let Some(stripped) = k.strip_prefix(&prefix) { targeted_params.insert(stripped.to_string(), v.clone()); }
                    }
                    match execute_read_operation(q, targeted_params, state.clone(), std::time::Instant::now(), 0.0, None).await {
                        Ok(res) => { 
                            if let Some(obj) = res.as_object() {
                                if let Some(pagination) = obj.get("pagination").and_then(|p| p.as_object()) {
                                    let pages = pagination.get("total_pages").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let items = pagination.get("total_items").and_then(|v| v.as_u64()).unwrap_or(0);
                                    if pages > max_pages { max_pages = pages; }
                                    total_items_sum += items;
                                }
                                if let Some(meta) = obj.get("meta").and_then(|m| m.as_object()) {
                                    execution_time_sum += meta.get("engine_time_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                }
                            }
                            results.insert(op_name.clone(), res); 
                        },
                        Err((code, res)) => {
                            results.insert(op_name.clone(), serde_json::json!({ "error": "Query failed", "status": code.as_u16(), "details": res }));
                        }
                    }
                },
                _ => { results.insert(op_name.clone(), serde_json::json!({ "error": "Only Read supported in Batch" })); }
            }
        }
    }

    let computed = match build_batch_computed_fields(&results, batch) {
        Ok(values) => values,
        Err(error) => {
            return Json(serde_json::json!({
                "error": error,
                "results": results,
                "batch_meta": {
                    "max_pages": max_pages,
                    "total_items_combined": total_items_sum,
                    "total_engine_time_ms": execution_time_sum,
                    "queries_count": batch.operations.len()
                }
            }));
        }
    };

    match batch.response_mode.as_deref() {
        Some("computed_only") => {
            return Json(serde_json::Value::Object(computed));
        }
        Some("merge_first_data") => {
            let Some(first_operation) = batch.operations.first() else {
                return Json(serde_json::json!({ "error": "batch has no operations" }));
            };
            let Some(source_result) = results.get(first_operation) else {
                return Json(serde_json::json!({ "error": format!("batch result '{}' not found", first_operation) }));
            };
            let merged = match merge_computed_into_result(source_result, &computed) {
                Ok(value) => value,
                Err(error) => return Json(serde_json::json!({ "error": error })),
            };
            if let Some(name) = cache_op_name {
                state.response_cache.put(name, &params, merged.clone());
            }
            return Json(merged);
        }
        _ => {}
    }

    Json(serde_json::json!({
        "results": results,
        "computed": computed,
        "batch_meta": { "max_pages": max_pages, "total_items_combined": total_items_sum, "total_engine_time_ms": execution_time_sum, "queries_count": batch.operations.len() }
    }))
}

fn merge_computed_into_result(
    source_result: &serde_json::Value,
    computed: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut merged = source_result.clone();
    let Some(object) = merged.as_object_mut() else {
        return Err("batch merge source must be an object response".to_string());
    };

    let Some(data) = object.get_mut("data") else {
        return Err("batch merge source has no data field".to_string());
    };

    let Some(items) = data.as_array_mut() else {
        return Err("batch merge source data must be an array".to_string());
    };

    for item in items {
        let Some(item_object) = item.as_object_mut() else {
            return Err("batch merge source data items must be objects".to_string());
        };
        let original = std::mem::take(item_object);
        let mut reordered = serde_json::Map::new();
        let mut inserted = false;

        for (key, value) in original {
            if key == "categoria" && !inserted {
                for (computed_key, computed_value) in computed {
                    reordered.insert(computed_key.clone(), computed_value.clone());
                }
                inserted = true;
            }
            reordered.insert(key, value);
        }

        if !inserted {
            for (computed_key, computed_value) in computed {
                reordered.insert(computed_key.clone(), computed_value.clone());
            }
        }

        *item_object = reordered;
    }

    Ok(merged)
}

fn build_batch_computed_fields(
    results: &serde_json::Map<String, serde_json::Value>,
    batch: &crate::core::saved_queries::SavedBatch,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut computed = serde_json::Map::new();

    for field in &batch.computed_fields {
        let parsed = crate::core::expression::parse_expression(&field.expression)
            .map_err(|error| format!("invalid batch computed expression '{}': {}", field.name, error))?;
        let mut context = HashMap::new();

        for (input_name, source) in &field.inputs {
            let value = resolve_batch_input(results, source)?;
            context.insert(input_name.clone(), value);
        }

        let value = crate::core::expression::evaluate(&parsed, &context);
        computed.insert(field.name.clone(), serde_json::json!(value));
    }

    Ok(computed)
}

fn resolve_batch_input(
    results: &serde_json::Map<String, serde_json::Value>,
    source: &str,
) -> Result<f64, String> {
    let (op_name, path) = source
        .split_once('.')
        .ok_or_else(|| format!("invalid computed input source '{}'", source))?;
    let result = results
        .get(op_name)
        .ok_or_else(|| format!("batch result '{}' not found", op_name))?;

    match path {
        "summary" => extract_aggregation_summary(result, 0),
        _ if path.starts_with("summary[") && path.ends_with(']') => {
            let index = path[8..path.len() - 1]
                .parse::<usize>()
                .map_err(|_| format!("invalid aggregation index in '{}'", source))?;
            extract_aggregation_summary(result, index)
        }
        _ => Err(format!("unsupported computed input path '{}'", source)),
    }
}

fn extract_aggregation_summary(result: &serde_json::Value, index: usize) -> Result<f64, String> {
    result
        .get("aggregations")
        .and_then(|value| value.as_array())
        .and_then(|items| items.get(index))
        .and_then(|value| value.get("summary"))
        .and_then(|value| value.as_f64())
        .ok_or_else(|| format!("aggregation summary at index {} not found", index))
}

async fn execute_read_operation(
    query: &crate::core::saved_queries::SavedQuery,
    params: HashMap<String, String>,
    state: Arc<ServerState>,
    start_time: std::time::Instant,
    ops_load_ms: f64,
    auth_context: Option<&AuthContext>,
) -> Result<serde_json::Value, (StatusCode, serde_json::Value)> {
    fn param_key(raw: &str) -> Option<&str> {
        raw.strip_prefix('$')
            .and_then(|spec| spec.split('|').next())
            .map(str::trim)
            .filter(|key| !key.is_empty())
    }

    let mut missing_params = Vec::new();
    let filters: Vec<Filter> = query.filters.iter().map(|sf| {
        let mut val = sf.value.clone();
        if let Some(key) = param_key(&val) {
            if let Some(param_val) = params.get(key) { val = param_val.clone(); }
            else { missing_params.push(key.to_string()); }
        }
        let value_to = sf.value_to.as_ref().map(|raw| {
            if let Some(key) = param_key(raw) {
                params.get(key).cloned().unwrap_or_else(|| raw.clone())
            } else {
                raw.clone()
            }
        });
        let value_options = sf.values.iter().map(|raw| {
            if let Some(key) = param_key(raw) {
                params.get(key).cloned().unwrap_or_else(|| raw.clone())
            } else {
                raw.clone()
            }
        }).collect();
        Filter { field: sf.field.clone(), op: ComparisonOp::from_str(&sf.op), value: val, value_to, value_options, field_type: sf.field_type }
    }).collect();
    
    let mut aggregations = query.aggregations.clone();
    for agg in &mut aggregations {
        if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
            for val in obj.values_mut() {
                if let Some(s) = val.as_str() {
                    if let Some(key) = param_key(s) {
                        if let Some(param_val) = params.get(key) {
                            if let Ok(num) = param_val.parse::<u64>() { *val = serde_json::json!(num); }
                            else { *val = serde_json::json!(param_val); }
                        } else { missing_params.push(key.to_string()); }
                    }
                }
            }
        }
    }

    if !missing_params.is_empty() {
         missing_params.sort(); missing_params.dedup();
         return Err((StatusCode::BAD_REQUEST, serde_json::json!({ "error": "Missing params", "missing": missing_params })));
    }

    let (engine_aggregations, collect_aggregations) = split_rest_collect_aggregations(&aggregations)
        .map_err(|error| (StatusCode::BAD_REQUEST, serde_json::json!({ "error": error })))?;

    let runtime_grouping = query.response_grouping.as_ref().map(|grouping| {
        let mut grouping = grouping.clone();
        let grouping_page_param = format!("{}_pagination", grouping.items_as);
        let grouping_limit = params
            .get("grouping_limit")
            .and_then(|value| value.parse::<usize>().ok())
            .or(grouping.limit_grouping)
            .unwrap_or(100)
            .min(100);
        let grouping_page = params
            .get("grouping_page")
            .or_else(|| params.get(&grouping_page_param))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let grouping_offset = params
            .get("grouping_offset")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or((grouping_page - 1) * grouping_limit);

        grouping.limit_grouping = Some(grouping_limit);
        grouping.offset_grouping = Some(grouping_offset);
        grouping
    });

    let filters_op = match query.filters_op.as_str() { "Or" => LogicalOp::Or, _ => LogicalOp::And };
    let order_by: Vec<OrderBy> = query.order_by.iter().map(|so| {
        OrderBy { field: so.field.clone(), direction: if so.direction == "Desc" { SortDirection::Desc } else { SortDirection::Asc } }
    }).collect();
    
    let param_fields: Vec<String> = params.get("fields").map(|s| s.split(',').map(|f| f.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    // Limit precedence (highest → lowest):
    //   1. ?limit=N query param (or whatever `limit_param` declared by the
    //      saved op, if any) — runtime override
    //   2. saved op's static `limit`
    //   3. default 100
    //
    // The hard upper bound below protects against unbounded scans on large
    // tables (a saved op with `limit: 10_000_000` could otherwise drag the
    // server). Saved ops that legitimately need more should paginate via
    // `?page=N`. The previous `.min(100)` was too strict — diagnostic
    // saved ops (e.g. `list-recent-checks` for stale detection) need to
    // see ~5k rows to compute aggregates client-side.
    const HARD_MAX_LIMIT: usize = 10_000;
    let limit = if let Some(ref param) = query.limit_param {
        let key = param_key(param).unwrap_or(param);
        params.get(key).and_then(|s| s.parse::<usize>().ok()).or(query.limit)
    } else { query.limit }.unwrap_or(100).min(HARD_MAX_LIMIT);
    let page = params
        .get("page")
        .or_else(|| params.get("pagination"))
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let offset = (page - 1) * limit;

    let state_search = state.clone();
    let sel_fields = query.selected_fields.clone();
    let aggs_query = engine_aggregations.clone();
    let uses_response_grouping = runtime_grouping.is_some();
    let engine_limit = if uses_response_grouping { 100 } else { limit };
    let engine_offset = if uses_response_grouping { 0 } else { offset };

    let setup_ms = start_time.elapsed().as_secs_f64() * 1000.0;
    let engine_start = std::time::Instant::now();
    let auth_ctx_clone = auth_context.cloned();
    let result = run_query_page(
        query.clone(),
        params.clone(),
        state_search,
        auth_ctx_clone,
        filters.clone(),
        filters_op,
        order_by.clone(),
        aggs_query.clone(),
        param_fields.clone(),
        sel_fields.clone(),
        engine_limit,
        engine_offset,
    ).await;

    let engine_total_ms = engine_start.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(query_result) => {
            let mapping_start = std::time::Instant::now();
            let headers = query_result.headers.clone();
            let source_total_found = query_result.total_found;
            let rows = materialize_query_rows(&query_result, state.clone(), query, &headers).await;
            let needs_full_source_rows = !collect_aggregations.is_empty() || uses_response_grouping;
            let mut source_rows = if needs_full_source_rows { Some(rows.clone()) } else { None };

            if let Some(ref mut all_rows) = source_rows {
                if source_total_found > all_rows.len() {
                    if source_total_found > MAX_GROUPED_RESPONSE_ROWS {
                        return Err((StatusCode::BAD_REQUEST, serde_json::json!({
                            "error": "Shaped response too large",
                            "details": format!("REST response shaping is limited to {} source rows", MAX_GROUPED_RESPONSE_ROWS)
                        })));
                    }

                    let mut next_offset = engine_offset + all_rows.len();
                    while next_offset < source_total_found {
                        let page_result = run_query_page(
                            query.clone(),
                            params.clone(),
                            state.clone(),
                            auth_context.cloned(),
                            filters.clone(),
                            filters_op,
                            order_by.clone(),
                            aggs_query.clone(),
                            param_fields.clone(),
                            sel_fields.clone(),
                            engine_limit,
                            next_offset,
                        ).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "error": e.to_string() })))?;
                        let page_rows = materialize_query_rows(&page_result, state.clone(), query, &headers).await;
                        if page_rows.is_empty() {
                            break;
                        }
                        next_offset += page_rows.len();
                        all_rows.extend(page_rows);
                        if all_rows.len() > MAX_GROUPED_RESPONSE_ROWS {
                            return Err((StatusCode::BAD_REQUEST, serde_json::json!({
                                "error": "Shaped response too large",
                                "details": format!("REST response shaping is limited to {} source rows", MAX_GROUPED_RESPONSE_ROWS)
                            })));
                        }
                    }
                }
            }

            let row_to_json = |h: &[String], r: &Vec<String>| {
                let mut m = serde_json::Map::new();
                for (i, head) in h.iter().enumerate() {
                    if let Some(val) = r.get(i) {
                        let first = if val.is_empty() { b'\0' } else { val.as_bytes()[0] };
                        let json_val = if !val.is_empty() && (first.is_ascii_digit() || first == b'-') {
                            if let Ok(n) = val.parse::<i64>() { serde_json::Value::Number(n.into()) }
                            else if let Ok(f) = val.parse::<f64>() { serde_json::Number::from_f64(f).map(serde_json::Value::Number).unwrap_or(serde_json::Value::String(val.clone())) }
                            else { serde_json::Value::String(val.clone()) }
                        } else { serde_json::Value::String(val.clone()) };
                        m.insert(head.clone(), json_val);
                    }
                }
                m
            };

            let data: Vec<_> = rows.into_par_iter().map(|r| row_to_json(&headers, &r)).collect();
            let shaped_data: Option<Vec<_>> = source_rows
                .map(|rows| rows.into_par_iter().map(|r| row_to_json(&headers, &r)).collect());
            let source_data = shaped_data.as_deref().unwrap_or(&data);
            let grouped_data = if let Some(grouping) = &runtime_grouping {
                Some(group_rows_by_field(source_data, grouping, Some(source_total_found)).map_err(|e| (StatusCode::BAD_REQUEST, serde_json::json!({ "error": e })))?)
            } else {
                None
            };
            let mut aggregations_data: Vec<serde_json::Value> = query_result.aggregations.map(|aggs| {
                aggs.into_iter().map(|agg| {
                    let agg_h = agg.headers;
                    let rows_j: Vec<_> = agg.rows.into_iter().map(|r| row_to_json(&agg_h, &r)).collect();
                    serde_json::json!({ "data": rows_j, "summary": agg.summary })
                }).collect()
            }).unwrap_or_default();

            for collect in &collect_aggregations {
                aggregations_data.push(
                    build_collect_aggregation(source_data, collect)
                        .map_err(|e| (StatusCode::BAD_REQUEST, serde_json::json!({ "error": e })))?
                );
            }

            let grouped_total_items = grouped_data
                .as_ref()
                .and_then(|value| value.as_array().map(|items| items.len()));
            let paged_grouped_data = grouped_data.and_then(|value| {
                value.as_array().map(|items| {
                    serde_json::Value::Array(
                        items
                            .iter()
                            .skip(offset)
                            .take(limit)
                            .cloned()
                            .collect(),
                    )
                })
            });

            let mut response = serde_json::Map::new();
            response.insert("data".to_string(), paged_grouped_data.unwrap_or_else(|| serde_json::json!(data)));
            if !aggregations_data.is_empty() { response.insert("aggregations".to_string(), serde_json::json!(aggregations_data)); }
            response.insert("meta".to_string(), serde_json::json!({ "engine_time_ms": query_result.execution_time_micros as f64 / 1000.0, "engine_total_ms": engine_total_ms, "ops_load_ms": ops_load_ms, "setup_ms": setup_ms, "mapping_ms": mapping_start.elapsed().as_secs_f64()*1000.0, "total_server_ms": start_time.elapsed().as_secs_f64()*1000.0, "total_found": query_result.total_found, "fields_count": headers.len(), "debug_info": query_result.debug_info }));
            if query_result.total_found > 0 && !headers.is_empty() {
                let total_items = if let Some(grouped_total_items) = grouped_total_items {
                    grouped_total_items
                } else {
                    query_result.total_found
                };
                let total_pages = if total_items == 0 { 1 } else { (total_items + limit - 1) / limit };
                response.insert("pagination".to_string(), serde_json::json!({
                    "page": page,
                    "per_page": limit,
                    "total_pages": total_pages,
                    "total_items": total_items
                }));
            }
            Ok(serde_json::Value::Object(response))
        },
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "error": e.to_string() })))
    }
}

async fn execute_split_enrichment_read(
    query: &crate::core::saved_queries::SavedQuery,
    profile: &crate::core::saved_queries::SavedSplitExecutionProfile,
    mut params: HashMap<String, String>,
    state: Arc<ServerState>,
    start_time: std::time::Instant,
    ops_load_ms: f64,
    auth_context: Option<&AuthContext>,
) -> Result<serde_json::Value, (StatusCode, serde_json::Value)> {
    if profile.mode != "split_enrichment" {
        return execute_read_operation(query, params, state, start_time, ops_load_ms, auth_context).await;
    }

    let mut base_query = query.clone();
    if !profile.base_join_aliases.is_empty() {
        base_query.joins.retain(|join| {
            let alias = join.alias.as_deref().unwrap_or(join.table.as_str());
            profile.base_join_aliases.iter().any(|configured| configured == alias)
        });
    }
    if !profile.base_select_aliases.is_empty() {
        base_query.select.retain(|field| {
            field
                .output_name
                .as_ref()
                .map(|name| profile.base_select_aliases.iter().any(|configured| configured == name))
                .unwrap_or(false)
        });
    }
    base_query.execution_profile = None;

    let mut base_result = execute_read_operation(
        &base_query,
        params.clone(),
        state.clone(),
        start_time,
        ops_load_ms,
        auth_context,
    )
    .await?;

    let Some(base_data) = base_result.get_mut("data").and_then(|value| value.as_array_mut()) else {
        return Ok(base_result);
    };
    if base_data.is_empty() {
        return Ok(base_result);
    }

    let transaccion_ids = base_data
        .iter()
        .filter_map(|item| item.get(&profile.key_field))
        .filter_map(value_to_id_token)
        .collect::<Vec<_>>();
    if transaccion_ids.is_empty() {
        return Ok(base_result);
    }
    params.insert(profile.ids_param.clone(), transaccion_ids.join(","));

    let mut enrichment_query = (*profile.enrichment_query).clone();
    enrichment_query.execution_profile = None;

    let enrichment_result = execute_read_operation(
        &enrichment_query,
        params,
        state,
        std::time::Instant::now(),
        0.0,
        auth_context,
    )
    .await?;
    let enrichment_rows = enrichment_result
        .get("data")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    let enrichment_key_field = profile.enrichment_key_field.as_ref().unwrap_or(&profile.key_field);
    let mut enrich_by_tx = HashMap::<String, serde_json::Value>::new();
    for row in enrichment_rows {
        if let Some(key) = row.get(enrichment_key_field).and_then(value_to_id_token) {
            enrich_by_tx.insert(key, row);
        } else {
            tracing::warn!("[split_enrichment] enrichment row missing key_field '{}', row keys: {:?}", enrichment_key_field, row.as_object().map(|o| o.keys().collect::<Vec<_>>()));
        }
    }
    tracing::info!("[split_enrichment] enrich_by_tx count={}", enrich_by_tx.len());

    for base_row in base_data.iter_mut() {
        if let Some(obj) = base_row.as_object_mut() {
            for (key, value) in &profile.defaults {
                obj.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }

        let Some(tx_id) = base_row.get(&profile.key_field).and_then(value_to_id_token) else {
            continue;
        };
        let Some(enriched) = enrich_by_tx.get(&tx_id) else {
            continue;
        };

        for field in &profile.merge_fields {
            if let Some(value) = enriched.get(field) {
                if let Some(obj) = base_row.as_object_mut() {
                    obj.insert(field.clone(), value.clone());
                }
            }
        }

        for additive in &profile.additive_fields {
            let base_discount = base_row
                .get(&additive.target_field)
                .and_then(value_to_f64)
                .unwrap_or(0.0);
            let bonus_discount = enriched
                .get(&additive.source_field)
                .and_then(value_to_f64)
                .unwrap_or(0.0);
            if let Some(obj) = base_row.as_object_mut() {
                if let Some(number) = serde_json::Number::from_f64(base_discount + bonus_discount) {
                    obj.insert(additive.target_field.clone(), serde_json::Value::Number(number));
                }
            }
        }
    }

    if let Some(meta) = base_result.get_mut("meta").and_then(|value| value.as_object_mut()) {
        meta.insert(
            "fields_count".to_string(),
            serde_json::json!(base_query.select.len()),
        );
        if let Some(debug) = meta.get("debug_info").and_then(|value| value.as_str()) {
            let debug_label = profile
                .debug_label
                .as_deref()
                .unwrap_or("split_mode: profile(split_enrichment)");
            meta.insert(
                "debug_info".to_string(),
                serde_json::Value::String(format!("{debug}; {debug_label}")),
            );
        }
    }

    Ok(base_result)
}

fn value_to_id_token(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn value_to_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

async fn run_query_page(
    query: crate::core::saved_queries::SavedQuery,
    params: HashMap<String, String>,
    state_search: Arc<ServerState>,
    auth_ctx_clone: Option<AuthContext>,
    filters: Vec<Filter>,
    filters_op: LogicalOp,
    order_by: Vec<OrderBy>,
    aggs_query: Vec<serde_json::Value>,
    param_fields: Vec<String>,
    sel_fields: Vec<String>,
    limit: usize,
    offset: usize,
) -> anyhow::Result<crate::core::types::QueryResult> {
    let query_entity = query.entity.clone();
    let query_table = query.table.clone();
    let query_owned = query.clone();

    tokio::task::spawn_blocking(move || {
        if query_owned.is_multi_table() {
            return crate::core::join_query::execute_join_query(
                &query_owned,
                &params,
                state_search.table_manager.clone(),
                if param_fields.is_empty() { None } else { Some(param_fields.clone()) },
                limit,
                offset,
                auth_ctx_clone.as_ref(),
            );
        }

        let t0 = std::time::Instant::now();
        let table_res = state_search.table_manager.get_table_for_query(&query_entity, &query_table);
        let t1 = std::time::Instant::now();
        match table_res {
            Ok(table_lock) => {
                let mut f_search = if !param_fields.is_empty() { param_fields }
                                   else if sel_fields.is_empty() && aggs_query.is_empty() {
                                       let all = Table::get_indexed_fields_static(&query_entity, &query_table);
                                       Table::get_base_fields_static(&all)
                                   } else { sel_fields };

                if f_search.iter().any(|f| f == "*") {
                    let needs_persist = {
                        let table_ro = table_lock.read().unwrap();
                        table_ro.manifest.original_fields.is_empty()
                    };
                    let all_cols = if needs_persist {
                        let mut cols = Table::get_indexed_fields_static(&query_entity, &query_table);
                        cols.retain(|f| !f.ends_with("_day") && !f.ends_with("_month") && !f.ends_with("_hour_bucket"));
                        if !cols.is_empty() {
                            let mut table_w = table_lock.write().unwrap();
                            if table_w.manifest.original_fields.is_empty() {
                                let _ = table_w.set_original_fields(cols.clone());
                            }
                        }
                        cols
                    } else {
                        table_lock.read().unwrap().manifest.original_fields.clone()
                    };
                    let mut new_f = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    for f in f_search {
                        if f == "*" {
                            for c in &all_cols {
                                if seen.insert(c.clone()) { new_f.push(c.clone()); }
                            }
                        } else if seen.insert(f.clone()) {
                            new_f.push(f);
                        }
                    }
                    f_search = new_f;
                }

                // Read lock: search/get_rows_batch only need &Table; using a write lock here
                // forced every read to wait behind CDC writes on hot tables.
                let table = table_lock.read().unwrap();
                let t2 = std::time::Instant::now();
                let mut res = table.search(&f_search, &filters, &filters_op, &aggs_query, &order_by, limit, offset, auth_ctx_clone.as_ref())?;
                let open_ms = t1.duration_since(t0).as_secs_f64() * 1000.0;
                let lock_ms = t2.duration_since(t1).as_secs_f64() * 1000.0;
                let extra = format!(" | Open: {:.2}ms, Lock: {:.2}ms", open_ms, lock_ms);
                if let Some(ref mut d) = res.debug_info { d.push_str(&extra); } else { res.debug_info = Some(extra); }
                Ok(res)
            }
            Err(e) => Err(e)
        }
    }).await.unwrap()
}

async fn materialize_query_rows(
    query_result: &crate::core::types::QueryResult,
    state: Arc<ServerState>,
    query: &crate::core::saved_queries::SavedQuery,
    headers: &[String],
) -> Vec<Vec<String>> {
    if query_result.rows.is_empty() && query_result.row_ids.is_some() {
        let ids = query_result.row_ids.as_ref().unwrap().clone();
        let tm = state.table_manager.clone();
        let entity = query.entity.clone();
        let table = query.table.clone();
        let h_inner = headers.to_vec();
        tokio::task::spawn_blocking(move || {
            let t_lock = tm.get_table_for_query(&entity, &table).unwrap();
            let t = t_lock.read().unwrap();
            t.get_rows_batch(&h_inner, &ids)
        }).await.unwrap().unwrap_or_default()
    } else {
        query_result.rows.clone()
    }
}

fn group_rows_by_field(
    data: &[serde_json::Map<String, serde_json::Value>],
    grouping: &crate::core::saved_queries::SavedResponseGrouping,
    source_total_override: Option<usize>,
) -> Result<serde_json::Value, String> {
    let group_fields = grouping.group_fields();
    if group_fields.is_empty() {
        return Err("response_grouping requires 'field' or 'fields'".to_string());
    }

    let mut grouped = Vec::<serde_json::Map<String, serde_json::Value>>::new();
    let mut index = std::collections::HashMap::<String, usize>::new();
    let child_grouping = grouping.children.first();

    for row in data {
        let mut row = row.clone();
        let mut parent_fields = Vec::with_capacity(group_fields.len());
        let mut key_parts = Vec::with_capacity(group_fields.len());
        for field in &group_fields {
            let value = row.get(field)
                .cloned()
                .ok_or_else(|| format!("Grouping field '{}' was not found in the response fields", field))?;
            key_parts.push(value.to_string());
            parent_fields.push((field.clone(), value));
        }

        let key = key_parts.join("\u{1f}");
        let item_row = if grouping.include_group_fields_in_items {
            row
        } else {
            for field in &group_fields {
                row.remove(field);
            }
            row
        };
        let is_empty = item_row.values().all(|v| {
            v.as_str().map_or(false, str::is_empty) || v.is_null()
        });

        if let Some(existing_index) = index.get(&key).copied() {
            if !is_empty {
                let group = grouped.get_mut(existing_index).unwrap();
                let items = group.get_mut(&grouping.items_as).and_then(|value| value.as_array_mut()).ok_or_else(|| "Invalid grouped response state".to_string())?;
                items.push(serde_json::Value::Object(item_row));
            }
        } else {
            let mut group = serde_json::Map::new();
            for (field, value) in parent_fields {
                group.insert(field, value);
            }
            let items = if is_empty { Vec::new() } else { vec![serde_json::Value::Object(item_row)] };
            group.insert(grouping.items_as.clone(), serde_json::Value::Array(items));
            index.insert(key, grouped.len());
            grouped.push(group);
        }
    }

    // Apply pagination to grouped items
    let limit = grouping.limit_grouping.unwrap_or(100).min(100);
    let offset = grouping.offset_grouping.unwrap_or(0);
    let use_source_total_override = source_total_override.filter(|_| grouped.len() == 1);
    for group in &mut grouped {
        if let Some(items) = group.get_mut(&grouping.items_as).and_then(|value| value.as_array_mut()) {
            let total_items = use_source_total_override.unwrap_or(items.len());
            let paged: Vec<_> = if limit == 0 {
                Vec::new()
            } else {
                items.iter().skip(offset).take(limit).cloned().collect()
            };
            let page = if limit == 0 { 1 } else { (offset / limit) + 1 };
            let total_pages = if limit == 0 { 1 } else { (total_items + limit - 1) / limit };
            *items = paged;
            group.insert(format!("{}_pagination", grouping.items_as), serde_json::json!({
                "page": page,
                "per_page": limit,
                "total_pages": total_pages,
                "total_items": total_items
            }));
        }
    }

    if let Some(child) = child_grouping {
        for group in &mut grouped {
            let items_value = group.get_mut(&grouping.items_as).ok_or_else(|| "Invalid grouped response state".to_string())?;
            let child_rows = items_value
                .as_array()
                .ok_or_else(|| "Invalid grouped response state".to_string())?
                .iter()
                .map(|value| value.as_object().cloned().ok_or_else(|| "Invalid grouped response item state".to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            *items_value = group_rows_by_field(&child_rows, child, None)?;
        }
    }

    Ok(serde_json::Value::Array(grouped.into_iter().map(serde_json::Value::Object).collect()))
}

fn split_rest_collect_aggregations(
    aggregations: &[serde_json::Value],
) -> Result<(Vec<serde_json::Value>, Vec<SavedCollectAggregation>), String> {
    let mut engine_aggregations = Vec::new();
    let mut collect_aggregations = Vec::new();

    for aggregation in aggregations {
        match SavedCollectAggregation::from_aggregation(aggregation) {
            Ok(Some(collect)) => collect_aggregations.push(collect),
            Ok(None) => engine_aggregations.push(aggregation.clone()),
            Err(error) => {
                return Err(format!("Invalid Collect aggregation: {}", error));
            }
        }
    }

    Ok((engine_aggregations, collect_aggregations))
}

fn build_collect_aggregation(
    data: &[serde_json::Map<String, serde_json::Value>],
    collect: &SavedCollectAggregation,
) -> Result<serde_json::Value, String> {
    let group_fields = collect.group_fields();
    if group_fields.is_empty() {
        return Err("Collect requires 'group_by' or 'group_by_fields'".to_string());
    }
    if data.is_empty() {
        return Ok(serde_json::json!({
            "kind": "Collect",
            "items_as": collect.items_as,
            "data": [],
            "summary": 0
        }));
    }

    let item_fields = resolve_collect_item_fields(data, collect, &group_fields)?;
    let mut grouped = Vec::<serde_json::Map<String, serde_json::Value>>::new();
    let mut index = std::collections::HashMap::<String, usize>::new();
    let mut total_items = 0usize;

    for row in data {
        let mut parent_fields = Vec::with_capacity(group_fields.len());
        let mut key_parts = Vec::with_capacity(group_fields.len());
        for field in &group_fields {
            let value = row
                .get(field)
                .cloned()
                .ok_or_else(|| format!("Collect group field '{}' was not found in the projected response", field))?;
            key_parts.push(value.to_string());
            parent_fields.push((field.clone(), value));
        }

        let item = build_collect_item(row, &item_fields, collect.include_group_fields_in_items)?;
        let key = key_parts.join("\u{1f}");

        if let Some(existing_index) = index.get(&key).copied() {
            let group = grouped.get_mut(existing_index).unwrap();
            let items = group
                .get_mut(&collect.items_as)
                .and_then(|value| value.as_array_mut())
                .ok_or_else(|| "Invalid Collect aggregation state".to_string())?;
            items.push(item);
        } else {
            let mut group = serde_json::Map::new();
            for (field, value) in parent_fields {
                group.insert(field, value);
            }
            group.insert(collect.items_as.clone(), serde_json::Value::Array(vec![item]));
            index.insert(key, grouped.len());
            grouped.push(group);
        }

        total_items += 1;
    }

    if !collect.order_by.is_empty() {
        for group in &mut grouped {
            let Some(items) = group.get_mut(&collect.items_as).and_then(|value| value.as_array_mut()) else {
                continue;
            };
            items.sort_by(|left, right| compare_collect_values(left, right, &collect.order_by));
        }
    }

    Ok(serde_json::json!({
        "kind": "Collect",
        "items_as": collect.items_as,
        "data": grouped.into_iter().map(serde_json::Value::Object).collect::<Vec<_>>(),
        "summary": total_items
    }))
}

fn resolve_collect_item_fields(
    data: &[serde_json::Map<String, serde_json::Value>],
    collect: &SavedCollectAggregation,
    group_fields: &[String],
) -> Result<Vec<String>, String> {
    let available_fields = data
        .first()
        .map(|row| row.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    let item_fields = if collect.item_fields.is_empty() {
        available_fields
            .into_iter()
            .filter(|field| collect.include_group_fields_in_items || !group_fields.contains(field))
            .collect::<Vec<_>>()
    } else {
        collect.item_fields.clone()
    };

    if item_fields.is_empty() {
        return Err("Collect requires projected item fields or a non-empty row shape".to_string());
    }

    for field in &item_fields {
        if !data.iter().all(|row| row.contains_key(field)) {
            return Err(format!("Collect item field '{}' was not found in the projected response", field));
        }
    }

    for order in &collect.order_by {
        if !item_fields.iter().any(|field| field == &order.field) {
            return Err(format!("Collect order_by field '{}' must also be present in item_fields", order.field));
        }
    }

    Ok(item_fields)
}

fn build_collect_item(
    row: &serde_json::Map<String, serde_json::Value>,
    item_fields: &[String],
    include_group_fields_in_items: bool,
) -> Result<serde_json::Value, String> {
    let mut item = serde_json::Map::new();

    for field in item_fields {
        let value = row
            .get(field)
            .cloned()
            .ok_or_else(|| format!("Collect item field '{}' was not found in the projected response", field))?;
        item.insert(field.clone(), value);
    }

    if include_group_fields_in_items {
        for (field, value) in row {
            item.entry(field.clone()).or_insert_with(|| value.clone());
        }
    }

    Ok(serde_json::Value::Object(item))
}

fn compare_collect_values(
    left: &serde_json::Value,
    right: &serde_json::Value,
    order_by: &[crate::core::saved_queries::SavedOrderBy],
) -> std::cmp::Ordering {
    let left_object = left.as_object();
    let right_object = right.as_object();

    for order in order_by {
        let left_value = left_object.and_then(|object| object.get(&order.field));
        let right_value = right_object.and_then(|object| object.get(&order.field));
        let ordering = compare_collect_scalar(left_value, right_value);
        if ordering != std::cmp::Ordering::Equal {
            return if order.direction == "Desc" {
                ordering.reverse()
            } else {
                ordering
            };
        }
    }

    std::cmp::Ordering::Equal
}

fn compare_collect_scalar(
    left: Option<&serde_json::Value>,
    right: Option<&serde_json::Value>,
) -> std::cmp::Ordering {
    match (left, right) {
        (Some(serde_json::Value::Number(left_number)), Some(serde_json::Value::Number(right_number))) => left_number
            .as_f64()
            .and_then(|left_float| right_number.as_f64().and_then(|right_float| left_float.partial_cmp(&right_float)))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(serde_json::Value::String(left_string)), Some(serde_json::Value::String(right_string))) => left_string.cmp(right_string),
        (Some(left_value), Some(right_value)) => left_value.to_string().cmp(&right_value.to_string()),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

async fn handle_insert(def: &crate::core::saved_queries::SavedInsert, payload: HashMap<String, String>, state: Arc<ServerState>) -> impl IntoResponse {
    if !def.expected_fields.is_empty() {
        let mut missing = Vec::new();
        for f in &def.expected_fields { if !payload.contains_key(f) { missing.push(f.clone()); } }
        if !missing.is_empty() { return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Missing fields", "missing": missing }))); }
    }
    let result = match state.table_manager.get_table(&def.entity, &def.table) {
        Ok(t_lock) => { let mut t = t_lock.write().unwrap(); t.insert(payload) },
        Err(e) => Err(e)
    };
    match result {
        Ok(_) => (StatusCode::CREATED, Json(serde_json::json!({ "status": "success", "message": "Record inserted" }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))
    }
}
