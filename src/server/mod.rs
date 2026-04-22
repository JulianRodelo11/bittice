pub mod grpc;
pub mod table_manager;
pub mod logging;

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
use tokio::sync::{oneshot, RwLock as TokioRwLock};
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

pub fn show_banner_with_filter(filter: Option<String>) {
    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[1mBittice Query Engine\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mThe engine is running and ready for requests.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m  \x1b[1mREST API:\x1b[0m    http://0.0.0.0:3000");
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
    println!("\x1b[32m◆\x1b[0m  \x1b[1mConfig API (REST):\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m GET    /_config             (List all)");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m GET    /_config?name=...    (View definition)");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m POST   /_config             (Create)");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m PUT    /_config             (Edit)");
    println!("\x1b[90m│\x1b[0m  \x1b[32m•\x1b[0m DELETE /_config?name=...    (Delete)");
}

pub fn show_banner() {
    show_banner_with_filter(None);
}

pub(crate) async fn wait_for_exit(shutdown_tx: Option<oneshot::Sender<()>>) -> anyhow::Result<()> {
    tokio::signal::ctrl_c().await?;
    println!("\x1b[34m│\x1b[0m");
    println!("\x1b[33m▲\x1b[0m  \x1b[1mShutting down\x1b[0m");
    println!("\x1b[34m│\x1b[0m  \x1b[90mStopping Bittice engine safely...\x1b[0m");
    println!("\x1b[34m└\x1b[0m\n");
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }
    Ok(())
}
pub async fn start_all_servers(entity_filter: Option<String>) -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let table_manager = Arc::new(TableManager::new());
    let active_workers = Arc::new(StdRwLock::new(HashSet::new()));
    
    // Convert entity_filter to lowercase and trim it
    let entity_filter = entity_filter.map(|e| e.trim().to_lowercase());
    
    if let Some(ref f) = entity_filter {
        debug!("Filtering by entity: '{}'", f);
    } else {
        debug!("No entity filter applied (loading all)");
    }
    
    // --- AUTO-START CDC WORKERS ---
    scan_and_start_cdc(table_manager.clone(), entity_filter.clone(), active_workers.clone());

    let http_tm = table_manager.clone();
    let http_filter = entity_filter.clone();
    let http_active = active_workers.clone();
    tokio::spawn(async move {
        start_server(http_tm, http_filter, http_active, shutdown_rx).await;
    });

    let grpc_tm = table_manager.clone();
    let grpc_filter = entity_filter.clone();
    tokio::spawn(async move {
        let _ = grpc::start_grpc_server_with_manager(50051, grpc_tm, grpc_filter, None).await;
    });

    show_banner();
    println!("\x1b[34m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[90mPress Ctrl+C to stop the server\x1b[0m");
    wait_for_exit(Some(shutdown_tx)).await
}

use std::collections::HashSet;
use std::sync::RwLock as StdRwLock;

pub struct ServerState {
    pub table_manager: Arc<TableManager>,
    pub ops_cache: Arc<TokioRwLock<Option<(Instant, Arc<Vec<SavedOperation>>)>>>,
    pub entity_filter: Option<String>,
    pub auth_service: crate::core::auth::AuthService,
    pub active_workers: Arc<StdRwLock<HashSet<String>>>,
}

pub fn scan_and_start_cdc(
    table_manager: Arc<TableManager>, 
    entity_filter: Option<String>,
    active_workers: Arc<StdRwLock<HashSet<String>>>
) {
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
                            
                            // Check if worker is already active
                            let entity_key = entity.clone();
                            {
                                let active = active_workers.read().unwrap();
                                if active.contains(&entity_key) {
                                    continue;
                                }
                            }

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

                            if let Some(vpn_path) = config["vpn_file"].as_str() {
                                if !vpn_path.trim().is_empty() {
                                    info!("CDC: Auto-starting VPN from saved config for entity '{}'...", entity);
                                    match crate::core::vpn::VpnManager::prepare_ovpn_file(vpn_path, &host) {
                                        Ok(prepared) => {
                                            if let Err(e) = crate::core::vpn::VpnManager::start(&prepared) {
                                                warn!("CDC: Failed to start VPN for entity '{}': {}", entity, e);
                                            }
                                        }
                                        Err(e) => {
                                            warn!("CDC: Failed to prepare VPN file for entity '{}': {}", entity, e);
                                        }
                                    }
                                }
                            }

                            let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
                            if (host == "localhost" || host == "0.0.0.0") && is_docker {
                                host = "host.docker.internal".to_string();
                            }

                            let url = format!("mysql://{}:{}@{}:{}/{}", user, pass, host, port, db);
                            let worker_tm = table_manager.clone();
                            let worker_entity = entity.clone();
                            let worker_db = db.clone();

                            // Mark as active
                            {
                                let mut active = active_workers.write().unwrap();
                                active.insert(entity_key);
                            }

                            std::thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().unwrap();
                                let db_name_for_log = worker_db.clone();
                                let worker = crate::core::cdc::CdcWorker::with_manager(
                                    url, 
                                    worker_entity, 
                                    worker_db, 
                                    worker_tm, 
                                );
                                if let Err(e) = rt.block_on(worker.run()) {
                                    error!("CDC: Worker for '{}' failed: {}", db_name_for_log, e);
                                }
                            });
                        }
                    }
                }
            }
        }
    }
}

pub async fn start_server(
    table_manager: Arc<TableManager>, 
    entity_filter: Option<String>, 
    active_workers: Arc<StdRwLock<HashSet<String>>>,
    shutdown_rx: oneshot::Receiver<()>
) {
    let state = Arc::new(ServerState {
        table_manager: table_manager.clone(),
        ops_cache: Arc::new(TokioRwLock::new(None)),
        entity_filter: entity_filter.clone(),
        auth_service: crate::core::auth::AuthService::new(table_manager),
        active_workers,
    });

    // Middleware de autenticación
    let auth_layer = axum::middleware::from_fn_with_state(state.clone(), auth_middleware);

    // Definir rutas: Catch-all para cualquier método
    let app = Router::new()
        .route("/*path", any(handle_request))
        .layer(auth_layer) // Añadimos la capa de auth
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(state.clone());

    let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let addr = format!("{}:3000", host);
    
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Could not bind HTTP server to {}: {}", addr, e);
            return;
        }
    };
    info!("Server started on http://{}", addr);

    let initial_warmed = warm_saved_query_targets(state.clone()).await;
    if initial_warmed > 0 {
        debug!("Startup: Warmed {} tables before serving requests", initial_warmed);
    }
    
    // --- CACHE WARMING & MAINTENANCE ---
    let warm_state = state.clone();
    tokio::spawn(async move {
        loop {
            let start = std::time::Instant::now();
            let warmed = warm_saved_query_targets(warm_state.clone()).await;
            let elapsed = start.elapsed().as_millis();
            if warmed > 0 && elapsed > 100 {
                debug!("Maintenance: Warmed {} tables in {}ms", warmed, elapsed);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        }
    });

    // Start Axum server with shutdown receiver
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await;
}

pub async fn auth_middleware(
    _state: State<Arc<ServerState>>,
    _headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // El middleware ahora es transparente. 
    // La resolución de identidad se hace bajo demanda en handle_request
    // usando la configuración específica de la query guardada.
    Ok(next.run(request).await)
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
    
    // Obtener token directamente de los headers para evitar problemas con extensiones
    let raw_auth_token = headers.get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());

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

    if path == "/_config/reload" {
        info!("Hot-reloading configuration from disk...");
        scan_and_start_cdc(state.table_manager.clone(), state.entity_filter.clone(), state.active_workers.clone());
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
            // Check for custom AuthConfig in the operation
            let mut effective_auth_ctx = auth_context.cloned();
            if let SavedOperation::Read(ref q) = op {
                if let Some(auth_cfg) = &q.auth_config {
                    if auth_cfg.enabled {
                        if let Some(token) = &raw_auth_token {
                            debug!("Using custom AuthConfig for operation '{}' (table: {})", op_name, auth_cfg.table);
                            effective_auth_ctx = state.auth_service.resolve_token(&q.entity, token, Some(auth_cfg)).await;

                            
                            // VALIDACIÓN ESTRICTA: Si no se pudo resolver el token (token inválido o usuario inexistente)
                            if effective_auth_ctx.is_none() {
                                warn!("-> 401 Unauthorized (Identity resolution failed for {})", op_name);
                                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ 
                                    "error": "Unauthorized", 
                                    "details": "Identity could not be resolved with the provided token." 
                                }))).into_response();
                            }
                        } else {
                            // VALIDACIÓN ESTRICTA: Si falta el token por completo
                            warn!("-> 401 Unauthorized (No token provided for {})", op_name);
                            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ 
                                "error": "Unauthorized", 
                                "details": "Bearer token is required for this operation." 
                            }))).into_response();
                        }
                    }
                }
            }

            match (method, op) {
                (Method::GET, SavedOperation::Read(ref q)) => {
                    match execute_read_operation(q, query_params, state, start_total, ops_load_ms, effective_auth_ctx.as_ref()).await {
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
    let limit = if let Some(ref param) = query.limit_param {
        let key = param_key(param).unwrap_or(param);
        params.get(key).and_then(|s| s.parse::<usize>().ok()).or(query.limit)
    } else { query.limit }.unwrap_or(100).min(100);
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

fn split_alias_field(value: &str, base_alias: &str) -> Option<(String, String)> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return None;
    }

    let mut parts = trimmed.splitn(2, '.');
    let first = parts.next()?.trim();
    match parts.next().map(str::trim) {
        Some(field) if !field.is_empty() => Some((first.to_string(), field.to_string())),
        _ => Some((base_alias.to_string(), first.to_string())),
    }
}

fn collect_warm_targets(ops: &[SavedOperation]) -> HashMap<(String, String), std::collections::HashSet<String>> {
    let mut targets: HashMap<(String, String), std::collections::HashSet<String>> = HashMap::new();

    for op in ops {
        if let SavedOperation::Read(q) = op {
            let base_alias = q.base_alias();
            for f in &q.selected_fields {
                if f != "*" {
                    if let Some((alias, field)) = split_alias_field(f, &base_alias) {
                        if alias == base_alias {
                            targets.entry((q.entity.clone(), q.table.clone())).or_default().insert(field);
                        }
                    }
                }
            }
            for s in &q.select {
                if let Some((alias, field)) = split_alias_field(&s.field, &base_alias) {
                    if alias == base_alias {
                        targets.entry((q.entity.clone(), q.table.clone())).or_default().insert(field);
                    }
                }
            }
            for f in &q.filters {
                if f.field != "?" {
                    if let Some((alias, field)) = split_alias_field(&f.field, &base_alias) {
                        let target_table = if alias == base_alias {
                            Some((q.entity.clone(), q.table.clone()))
                        } else {
                            q.joins.iter().find(|join| join.alias.as_deref().unwrap_or(join.table.as_str()) == alias).map(|join| (join.entity.clone().unwrap_or_else(|| q.entity.clone()), join.table.clone()))
                        };
                        if let Some(key) = target_table {
                            targets.entry(key).or_default().insert(field);
                        }
                    }
                }
            }
            for o in &q.order_by {
                if let Some((alias, field)) = split_alias_field(&o.field, &base_alias) {
                    let target_table = if alias == base_alias {
                        Some((q.entity.clone(), q.table.clone()))
                    } else {
                        q.joins.iter().find(|join| join.alias.as_deref().unwrap_or(join.table.as_str()) == alias).map(|join| (join.entity.clone().unwrap_or_else(|| q.entity.clone()), join.table.clone()))
                    };
                    if let Some(key) = target_table {
                        targets.entry(key).or_default().insert(field);
                    }
                }
            }
            for join in &q.joins {
                let join_alias = join.alias.as_deref().unwrap_or(join.table.as_str()).to_string();
                let join_entity = join.entity.clone().unwrap_or_else(|| q.entity.clone());
                let join_entry = targets.entry((join_entity, join.table.clone())).or_default();
                for cond in &join.on {
                    if let Some((alias, field)) = split_alias_field(&cond.left, &base_alias) {
                        if alias == join_alias {
                            join_entry.insert(field);
                        }
                    }
                    if let Some((alias, field)) = split_alias_field(&cond.right, &base_alias) {
                        if alias == join_alias {
                            join_entry.insert(field);
                        }
                    }
                }
            }
        }
    }

    targets
}

async fn warm_saved_query_targets(state: Arc<ServerState>) -> usize {
    let Ok(ops) = crate::core::saved_queries::load_operations_with_filter(state.entity_filter.clone()) else {
        return 0;
    };
    let targets = collect_warm_targets(&ops);
    if targets.is_empty() {
        return 0;
    }

    tokio::task::spawn_blocking(move || {
        let mut warmed_count = 0;
        for ((entity, table_name), fields_set) in targets {
            if let Ok(table_lock) = state.table_manager.get_table(&entity, &table_name) {
                let fields: Vec<String> = fields_set.into_iter().collect();
                let table = table_lock.read().unwrap();
                let _ = table.warm_up(&fields);
                warmed_count += 1;
            }
        }
        warmed_count
    }).await.unwrap_or(0)
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
            let t_lock = tm.get_table(&entity, &table).unwrap();
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
        let item = serde_json::Value::Object(item_row);

        if let Some(existing_index) = index.get(&key).copied() {
            let group = grouped.get_mut(existing_index).unwrap();
            let items = group.get_mut(&grouping.items_as).and_then(|value| value.as_array_mut()).ok_or_else(|| "Invalid grouped response state".to_string())?;
            items.push(item);
        } else {
            let mut group = serde_json::Map::new();
            for (field, value) in parent_fields {
                group.insert(field, value);
            }
            group.insert(grouping.items_as.clone(), serde_json::Value::Array(vec![item]));
            index.insert(key, grouped.len());
            grouped.push(group);
        }
    }

    // Aplicar paginación a los items agrupados
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
