pub mod grpc;
pub mod table_manager;

use axum::{
    debug_handler,
    extract::{State, Query},
    response::{IntoResponse, Json},
    routing::{any},
    Router,
    http::{Method, StatusCode},
};
use std::sync::{Arc};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot, RwLock as TokioRwLock};
use tower_http::trace::TraceLayer;
use tower_http::catch_panic::CatchPanicLayer;
use crate::core::saved_queries::{load_operations, SavedOperation};
use crate::core::types::{Filter, LogicalOp, ComparisonOp, SortDirection, OrderBy};
use std::collections::HashMap;
use rayon::prelude::*;
use crate::core::storage::table::Table;
use crate::server::table_manager::TableManager;

pub fn show_banner() {
    println!("\n  \x1b[1m\x1b[34mBittice Query Engine is active\x1b[0m");
    println!("  ----------------------------------------");
    println!("  \x1b[1mREST API:\x1b[0m    http://0.0.0.0:3000");
    println!("  \x1b[1mgRPC API:\x1b[0m    0.0.0.0:50051");
    println!("  ----------------------------------------");
    
    // Show saved queries
    if let Ok(ops) = load_operations() {
        if !ops.is_empty() {
            println!("  \x1b[1mLoaded queries:\x1b[0m");
            for op in ops {
                println!("    • /{}", op.name());
            }
            println!("  ----------------------------------------");
        }
    }

    println!("  \x1b[1mDynamic configuration:\x1b[0m");
    println!("  GET    /_config             (List all)");
    println!("  GET    /_config?name=...    (View definition)");
    println!("  POST   /_config             (Create)");
    println!("  PUT    /_config             (Edit)");
    println!("  DELETE /_config?name=...    (Delete)");
    println!("  ----------------------------------------");
    println!("  Press Ctrl+C to stop the server\n");
}

pub(crate) async fn wait_for_exit(shutdown_tx: Option<oneshot::Sender<()>>) -> anyhow::Result<()> {
    tokio::signal::ctrl_c().await?;
    println!("\n  \x1b[33m•\x1b[0m Shutting down Bittice...");
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }
    Ok(())
}

pub async fn start_all_servers(entity_filter: Option<String>) -> anyhow::Result<()> {
    let (log_tx, mut log_rx) = mpsc::channel::<String>(100);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let table_manager = Arc::new(TableManager::new());
    
    // Convert entity_filter to lowercase and trim it
    let entity_filter = entity_filter.map(|e| e.trim().to_lowercase());
    
    if let Some(ref f) = entity_filter {
        let _ = log_tx.try_send(format!("[DEBUG] Filtering by entity: '{}'", f));
    } else {
        let _ = log_tx.try_send("[DEBUG] No entity filter applied (loading all)".to_string());
    }
    
    // Task to print logs cleanly
    tokio::spawn(async move {
        while let Some(msg) = log_rx.recv().await {
            if !msg.starts_with("  ->") {
                println!("  \x1b[32m•\x1b[0m {}", msg);
            } else {
                println!("    \x1b[90m{}\x1b[0m", msg);
            }
        }
    });

    // --- AUTO-START CDC WORKERS ---
    let data_dir = std::path::Path::new("data");
    
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let entity_folder_name = entry.file_name().to_string_lossy().to_string();
                
                // If a filter is provided, skip this directory if it doesn't match
                if let Some(ref filter) = entity_filter {
                    if entity_folder_name.to_lowercase() != *filter {
                        continue;
                    }
                }

                let config_path = entry.path().join("cdc_config.json");
                
                if config_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&config_path) {
                        if let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) {
                            let user = config["user"].as_str().unwrap_or_default().to_string();
                            let pass = config["pass"].as_str().unwrap_or_default().to_string();
                            let mut host = config["host"].as_str().unwrap_or_default().to_string();
                            let db = config["database"].as_str().unwrap_or_default().to_string();
                            let entity = config["entity"].as_str().unwrap_or(&entity_folder_name).to_string();
                            
                            // If a filter is provided, also check against the 'entity' field in JSON
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

                            let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
                            if (host == "localhost" || host == "0.0.0.0") && is_docker {
                                host = "host.docker.internal".to_string();
                            }

                            let url = format!("mysql://{}:{}@{}:{}/{}", user, pass, host, port, db);
                            let worker_tm = table_manager.clone();
                            let worker_log = log_tx.clone();
                            let worker_entity = entity.clone();
                            let worker_db = db.clone();

                            let _ = log_tx.try_send(format!("[INFO] CDC: Initializing worker for '{}' (Host: {}, Port: {}, DB: {})", worker_entity, host, port, worker_db));
                            
                            std::thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().unwrap();
                                let db_name_for_log = worker_db.clone();
                                let error_log_tx = worker_log.clone();
                                let worker = crate::core::cdc::CdcWorker::with_manager(
                                    url, 
                                    worker_entity, 
                                    worker_db, 
                                    worker_tm, 
                                    Some(worker_log)
                                );
                                if let Err(e) = rt.block_on(worker.run()) {
                                    // Only log the failure if it hasn't been logged by the worker itself
                                    let err_msg = e.to_string();
                                    if !err_msg.contains("CDC_ERROR") {
                                        let _ = error_log_tx.try_send(format!("CDC_ERROR: Worker for '{}' failed: {}", db_name_for_log, err_msg));
                                    }
                                }
                            });
                        }
                    }
                }
            }
        }
    }

    let http_log_tx = log_tx.clone();
    let http_tm = table_manager.clone();
    let http_filter = entity_filter.clone();
    tokio::spawn(async move {
        start_server(http_log_tx, http_tm, http_filter, shutdown_rx).await;
    });

    let grpc_tm = table_manager.clone();
    let grpc_filter = entity_filter.clone();
    tokio::spawn(async move {
        let _ = grpc::start_grpc_server_with_manager(50051, grpc_tm, grpc_filter).await;
    });

    show_banner();
    wait_for_exit(Some(shutdown_tx)).await
}

pub struct ServerState {
    pub log_sender: mpsc::Sender<String>,
    pub table_manager: Arc<TableManager>,
    pub ops_cache: Arc<TokioRwLock<Option<(Instant, Arc<Vec<SavedOperation>>)>>>,
    pub entity_filter: Option<String>,
}

pub async fn start_server(log_sender: mpsc::Sender<String>, table_manager: Arc<TableManager>, entity_filter: Option<String>, shutdown_rx: oneshot::Receiver<()>) {
    let state = Arc::new(ServerState {
        log_sender: log_sender.clone(),
        table_manager,
        ops_cache: Arc::new(TokioRwLock::new(None)),
        entity_filter: entity_filter.clone(),
    });

    // Definir rutas: Catch-all para cualquier método
    let app = Router::new()
        .route("/*path", any(handle_request))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(state.clone());

    let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let addr = format!("{}:3000", host);
    
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            let _ = log_sender.send(format!("[ERROR] Could not bind HTTP server to {}: {}", addr, e)).await;
            return;
        }
    };
    let _ = log_sender.send(format!("Server started on http://{}", addr)).await;
    
    // --- CACHE WARMING & MAINTENANCE ---
    let warm_state = state.clone();
    let warm_logger = log_sender.clone();
    tokio::spawn(async move {
        loop {
            let start = std::time::Instant::now();
            if let Ok(ops) = crate::core::saved_queries::load_operations_with_filter(warm_state.entity_filter.clone()) {
                let mut targets: HashMap<(String, String), std::collections::HashSet<String>> = HashMap::new();
                for op in ops {
                    if let SavedOperation::Read(q) = op {
                        let entry = targets.entry((q.entity.clone(), q.table.clone())).or_default();
                        for f in &q.selected_fields { if f != "*" { entry.insert(f.clone()); } }
                        for f in &q.filters { if f.field != "?" { entry.insert(f.field.clone()); } }
                        for o in &q.order_by { entry.insert(o.field.clone()); }
                        for agg in &q.aggregations {
                            if let Some(obj) = agg.as_object() {
                                for val in obj.values() {
                                    if let Some(inner) = val.as_object() {
                                        if let Some(f) = inner.get("field").and_then(|v| v.as_str()) {
                                             if f != "?" { entry.insert(f.to_string()); }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !targets.is_empty() {
                    let warm_state_inner = warm_state.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        let mut warmed_count = 0;
                        for ((entity, table_name), fields_set) in targets {
                            if let Ok(table_lock) = warm_state_inner.table_manager.get_table(&entity, &table_name) {
                                let fields: Vec<String> = fields_set.into_iter().collect();
                                let table = table_lock.read().unwrap();
                                let _ = table.warm_up(&fields);
                                warmed_count += 1;
                            }
                        }
                        warmed_count
                    }).await;
                    if let Ok(c) = res {
                        let elapsed = start.elapsed().as_millis();
                        if elapsed > 100 { 
                            let _ = warm_logger.try_send(format!("  -> Maintenance: Warmed {} tables in {}ms", c, elapsed));
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });

    // Start Axum server with shutdown receiver
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await;
}

#[debug_handler]
async fn handle_request(
    State(state): State<Arc<ServerState>>,
    method: Method,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let start_total = std::time::Instant::now();
    let path = uri.path().to_string();
    let query_params: HashMap<String, String> = Query::try_from_uri(&uri)
        .map(|Query(params)| params)
        .unwrap_or_default();
    
    let op_name = path.trim_start_matches('/').to_string();
    // Non-blocking log send to avoid hanging the request
    let _ = state.log_sender.try_send(format!("{} /{}", method, op_name));
    
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

    // Internal endpoints
    if path == "/_debug" {
        let mut debug_info = serde_json::Map::new();
        debug_info.insert("ops_loaded".to_string(), serde_json::json!(ops.len()));
        let entities = std::fs::read_dir("data").map(|d| d.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect::<Vec<_>>()).unwrap_or_default();
        debug_info.insert("entities_on_disk".to_string(), serde_json::json!(entities));
        return (StatusCode::OK, Json(serde_json::Value::Object(debug_info))).into_response();
    }

    if path == "/_entities" {
        let data_dir = std::path::Path::new("data");
        let mut catalog = serde_json::Map::new();
        if let Ok(entities) = std::fs::read_dir(data_dir) {
            for entity_entry in entities.flatten() {
                if entity_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let entity_name = entity_entry.file_name().to_string_lossy().to_string();
                    let mut tables = Vec::new();
                    if let Ok(table_entries) = std::fs::read_dir(entity_entry.path()) {
                        for table_entry in table_entries.flatten() {
                            if table_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                tables.push(table_entry.file_name().to_string_lossy().to_string());
                            }
                        }
                    }
                    catalog.insert(entity_name, serde_json::json!(tables));
                }
            }
        }
        return (StatusCode::OK, Json(serde_json::Value::Object(catalog))).into_response();
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
            match (method, op) {
                (Method::GET, SavedOperation::Read(ref q)) => {
                    match execute_read_operation(q, query_params, state, start_total, ops_load_ms).await {
                        Ok(val) => (StatusCode::OK, Json(val)).into_response(),
                        Err((status, val)) => (status, Json(val)).into_response(),
                    }
                },
                (Method::GET, SavedOperation::Batch(ref b)) => {
                    handle_batch(b, query_params, state).await.into_response()
                },
                (Method::POST, SavedOperation::Insert(ref i)) => {
                    let payload: HashMap<String, String> = serde_json::from_slice(&body).unwrap_or_default();
                    handle_insert(i, payload, state).await.into_response()
                },
                (m, _) => {
                    let _ = state.log_sender.send(format!("  -> 405 Method Not Allowed ({})", m)).await;
                    (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({ "error": "Method not allowed for this operation" }))).into_response()
                }
            }
        } else {
            let _ = state.log_sender.send(format!("  -> 404 Not Found ('{}')", op_name)).await;
            (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("Operation '{}' not found", op_name) }))).into_response()
        }
    }
}

async fn handle_batch(
    batch: &crate::core::saved_queries::SavedBatch,
    params: HashMap<String, String>,
    state: Arc<ServerState>,
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
                    match execute_read_operation(q, targeted_params, state.clone(), std::time::Instant::now(), 0.0).await {
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

    Json(serde_json::json!({
        "results": results,
        "batch_meta": { "max_pages": max_pages, "total_items_combined": total_items_sum, "total_engine_time_ms": execution_time_sum, "queries_count": batch.operations.len() }
    }))
}

async fn execute_read_operation(
    query: &crate::core::saved_queries::SavedQuery,
    params: HashMap<String, String>,
    state: Arc<ServerState>,
    start_time: std::time::Instant,
    ops_load_ms: f64,
) -> Result<serde_json::Value, (StatusCode, serde_json::Value)> {
    let mut missing_params = Vec::new();
    let filters: Vec<Filter> = query.filters.iter().map(|sf| {
        let mut val = sf.value.clone();
        if val.starts_with('$') {
            let key = &val[1..];
            if let Some(param_val) = params.get(key) { val = param_val.clone(); }
            else { missing_params.push(key.to_string()); }
        }
        Filter { field: sf.field.clone(), op: ComparisonOp::from_str(&sf.op), value: val, value_options: vec![], field_type: sf.field_type }
    }).collect();
    
    let mut aggregations = query.aggregations.clone();
    for agg in &mut aggregations {
        if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
            for val in obj.values_mut() {
                if let Some(s) = val.as_str() {
                    if let Some(key) = s.strip_prefix('$') {
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

    let filters_op = match query.filters_op.as_str() { "Or" => LogicalOp::Or, _ => LogicalOp::And };
    let order_by: Vec<OrderBy> = query.order_by.iter().map(|so| {
        OrderBy { field: so.field.clone(), direction: if so.direction == "Desc" { SortDirection::Desc } else { SortDirection::Asc } }
    }).collect();
    
    let param_fields: Vec<String> = params.get("fields").map(|s| s.split(',').map(|f| f.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let limit = if let Some(ref param) = query.limit_param {
        let key = param.strip_prefix('$').unwrap_or(param);
        params.get(key).and_then(|s| s.parse::<usize>().ok()).or(query.limit)
    } else { query.limit }.unwrap_or(100).min(100);
    let page = params.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
    let offset = (page - 1) * limit;

    let query_entity = query.entity.clone();
    let query_table = query.table.clone();
    let state_search = state.clone();
    let sel_fields = query.selected_fields.clone();
    let aggs_query = query.aggregations.clone();

    let setup_ms = start_time.elapsed().as_secs_f64() * 1000.0;
    let engine_start = std::time::Instant::now();

    let result = tokio::task::spawn_blocking(move || {
        let t0 = std::time::Instant::now();
        let table_res = state_search.table_manager.get_table(&query_entity, &query_table);
        let t1 = std::time::Instant::now();
        match table_res {
            Ok(table_lock) => {
                let mut table = table_lock.write().unwrap();
                let _ = table.reload_if_needed();
                let mut f_search = if !param_fields.is_empty() { param_fields }
                                   else if sel_fields.is_empty() && aggs_query.is_empty() {
                                       let all = Table::get_indexed_fields_static(&query_entity, &query_table);
                                       Table::get_base_fields_static(&all)
                                   } else { sel_fields };

                if f_search.iter().any(|f| f == "*") {
                    let mut all_cols = table.manifest.original_fields.clone();
                    if all_cols.is_empty() {
                        all_cols = Table::get_indexed_fields_static(&query_entity, &query_table);
                        all_cols.retain(|f| !f.ends_with("_day") && !f.ends_with("_month") && !f.ends_with("_hour_bucket"));
                        if !all_cols.is_empty() { let _ = table.set_original_fields(all_cols.clone()); }
                    }
                    let mut new_f = Vec::new(); let mut seen = std::collections::HashSet::new();
                    for f in f_search {
                        if f == "*" { for c in &all_cols { if seen.insert(c.clone()) { new_f.push(c.clone()); } } }
                        else { if seen.insert(f.clone()) { new_f.push(f); } }
                    }
                    f_search = new_f;
                }

                let t2 = std::time::Instant::now();
                let mut res = table.search(&f_search, &filters, &filters_op, &aggs_query, &order_by, limit, offset)?;
                let open_ms = t1.duration_since(t0).as_secs_f64() * 1000.0;
                let lock_ms = t2.duration_since(t1).as_secs_f64() * 1000.0;
                let extra = format!(" | Open: {:.2}ms, Lock: {:.2}ms", open_ms, lock_ms);
                if let Some(ref mut d) = res.debug_info { d.push_str(&extra); } else { res.debug_info = Some(extra); }
                Ok(res)
            },
            Err(e) => Err(e)
        }
    }).await.unwrap();

    let engine_total_ms = engine_start.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(query_result) => {
            let mapping_start = std::time::Instant::now();
            let headers = query_result.headers.clone();
            let rows = if query_result.rows.is_empty() && query_result.row_ids.is_some() {
                let ids = query_result.row_ids.as_ref().unwrap().clone();
                let tm = state.table_manager.clone();
                let entity = query.entity.clone(); let table = query.table.clone();
                let h_inner = headers.clone();
                tokio::task::spawn_blocking(move || {
                    let t_lock = tm.get_table(&entity, &table).unwrap();
                    let t = t_lock.read().unwrap();
                    t.get_rows_batch(&h_inner, &ids)
                }).await.unwrap().unwrap_or_default()
            } else { query_result.rows };

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
            let aggregations_data: Option<Vec<_>> = query_result.aggregations.map(|aggs| {
                aggs.into_iter().map(|agg| {
                    let agg_h = agg.headers;
                    let rows_j: Vec<_> = agg.rows.into_iter().map(|r| row_to_json(&agg_h, &r)).collect();
                    serde_json::json!({ "data": rows_j, "summary": agg.summary })
                }).collect()
            });

            let mut response = serde_json::Map::new();
            response.insert("data".to_string(), serde_json::json!(data));
            if let Some(aggs) = aggregations_data { response.insert("aggregations".to_string(), serde_json::json!(aggs)); }
            response.insert("meta".to_string(), serde_json::json!({ "engine_time_ms": query_result.execution_time_micros as f64 / 1000.0, "engine_total_ms": engine_total_ms, "ops_load_ms": ops_load_ms, "setup_ms": setup_ms, "mapping_ms": mapping_start.elapsed().as_secs_f64()*1000.0, "total_server_ms": start_time.elapsed().as_secs_f64()*1000.0, "total_found": query_result.total_found, "fields_count": headers.len(), "debug_info": query_result.debug_info }));
            if query_result.total_found > 0 && !headers.is_empty() {
                response.insert("pagination".to_string(), serde_json::json!({ "page": page, "per_page": limit, "total_pages": (query_result.total_found + limit - 1) / limit, "total_items": query_result.total_found }));
            }
            Ok(serde_json::Value::Object(response))
        },
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "error": e.to_string() })))
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
