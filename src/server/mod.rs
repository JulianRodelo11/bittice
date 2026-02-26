pub mod grpc;
pub mod table_manager;

use axum::{
    debug_handler,
    extract::{State, Query, Request},
    response::{IntoResponse, Json},
    routing::{any},
    Router,
    http::{Method, StatusCode},
};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot};
use tower_http::trace::TraceLayer;
use crate::core::saved_queries::{load_operations, SavedOperation};
use crate::core::types::{Filter, LogicalOp, ComparisonOp, SortDirection, OrderBy};
use std::collections::HashMap;
use rayon::prelude::*;
use crate::server::table_manager::TableManager;

// Estructura para compartir estado con los handlers de Axum
struct ServerState {
    log_sender: mpsc::Sender<String>,
    table_manager: Arc<TableManager>,
    ops_cache: Arc<RwLock<Option<(std::time::Instant, Vec<SavedOperation>)>>>,
}

pub async fn start_server(log_sender: mpsc::Sender<String>, shutdown_rx: oneshot::Receiver<()>) {
    let state = Arc::new(ServerState {
        log_sender: log_sender.clone(),
        table_manager: Arc::new(TableManager::new()),
        ops_cache: Arc::new(RwLock::new(None)),
    });

    // Definir rutas: Catch-all para cualquier método
    let app = Router::new()
        .route("/*path", any(handle_request))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{}:3000", host);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let _ = log_sender.send(format!("Server started on http://{}", addr)).await;
    
    // --- CACHE WARMING & MAINTENANCE ---
    // Periodically re-warms tables used in saved queries to prevent OS page cache eviction.
    let warm_state = state.clone();
    let warm_logger = log_sender.clone();
    tokio::spawn(async move {
        loop {
            let start = std::time::Instant::now();
            // Reload operations from disk
            if let Ok(ops) = load_operations() {
                let mut targets: HashMap<(String, String), std::collections::HashSet<String>> = HashMap::new();
                
                for op in ops {
                    if let SavedOperation::Read(q) = op {
                        let entry = targets.entry((q.entity.clone(), q.table.clone())).or_default();
                        // Add selected fields
                        for f in &q.selected_fields { if f != "*" { entry.insert(f.clone()); } }
                        // Add filter fields
                        for f in &q.filters { if f.field != "?" { entry.insert(f.field.clone()); } }
                        // Add order by fields
                        for o in &q.order_by { entry.insert(o.field.clone()); }
                        // Add aggregation fields
                        for agg in &q.aggregations {
                            if let Some(obj) = agg.as_object() {
                                for val in obj.values() {
                                    if let Some(inner) = val.as_object() {
                                        if let Some(f) = inner.get("field").and_then(|v| v.as_str()) {
                                             if f != "?" { entry.insert(f.to_string()); }
                                        }
                                        // Handle expressions fields if needed? (Complex, skipping for now)
                                    }
                                }
                            }
                        }
                    }
                }
                
                if !targets.is_empty() {
                    let warm_state_inner = warm_state.clone();
                    
                    // Run blocking IO task
                    let res = tokio::task::spawn_blocking(move || {
                        let mut warmed_count = 0;
                        for ((entity, table_name), fields_set) in targets {
                            if let Ok(table_lock) = warm_state_inner.table_manager.get_table(&entity, &table_name) {
                                let fields: Vec<String> = fields_set.into_iter().collect();
                                let table = table_lock.read().unwrap();
                                if table.warm_up(&fields).is_ok() {
                                    warmed_count += 1;
                                }
                            }
                        }
                        warmed_count
                    }).await;

                    if let Ok(c) = res {
                        let elapsed = start.elapsed().as_millis();
                        // Only log if it took significant time (>100ms), to avoid noise
                        if elapsed > 100 { 
                            let _ = warm_logger.send(format!("  -> Maintenance: Warmed {} tables in {}ms", c, elapsed)).await;
                        }
                    }
                }
            }
            // Sleep for 5 minutes
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
    // -------------------------

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            shutdown_rx.await.ok();
        })
        .await
        .unwrap();
}

#[debug_handler]
async fn handle_request(
    State(state): State<Arc<ServerState>>,
    req: Request,
) -> impl IntoResponse {
    let start_total = std::time::Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query_params: HashMap<String, String> = Query::try_from_uri(req.uri())
        .map(|Query(params)| params)
        .unwrap_or_default();
    
    let op_name = path.trim_start_matches('/');
    let _ = state.log_sender.send(format!("{} /{}", method, op_name)).await;

    // Load operations from cache or disk
    let start_ops = std::time::Instant::now();
    let ops: Vec<SavedOperation> = {
        let mut cache = state.ops_cache.write().unwrap();
        let needs_reload = match &*cache {
            Some((ts, _)) => ts.elapsed().as_secs() > 5,
            None => true,
        };
        if needs_reload {
            let loaded = load_operations().unwrap_or_default();
            *cache = Some((std::time::Instant::now(), loaded.clone()));
            loaded
        } else {
            cache.as_ref().unwrap().1.clone()
        }
    };
    let ops_load_ms = start_ops.elapsed().as_secs_f64() * 1000.0;
    
    // Find operation by name
    let operation = ops.iter().find(|o| o.name() == op_name);

    if let Some(op) = operation {
        match (method, op) {
            (Method::GET, SavedOperation::Read(ref q)) => {
                handle_read(q, query_params, state, start_total, ops_load_ms).await.into_response()
            },
            (Method::GET, SavedOperation::Batch(ref b)) => {
                handle_batch(b, query_params, state).await.into_response()
            },
            (Method::POST, SavedOperation::Insert(ref i)) => {
                // Extract body as JSON
                let body_bytes = ax_body_to_bytes(req).await;
                let payload: HashMap<String, String> = serde_json::from_slice(&body_bytes).unwrap_or_default();
                handle_insert(i, payload, state).await.into_response()
            },
            // Fallbacks
            (m, _) => {
                let _ = state.log_sender.send(format!("  -> 405 Method Not Allowed ({})", m)).await;
                (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({
                    "error": "Method not allowed for this operation"
                }))).into_response()
            }
        }
    } else {
        let _ = state.log_sender.send(format!("  -> 404 Not Found ('{}')", op_name)).await;
        (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "Operation not found in CLI. Save it first."
        }))).into_response()
    }
}

// Helper to extract body bytes from Axum request
async fn ax_body_to_bytes(req: Request) -> Vec<u8> {
    use axum::body::to_bytes;
    let bytes = to_bytes(req.into_body(), 1024 * 1024).await.unwrap_or_default();
    bytes.to_vec()
}

async fn handle_read(
    query: &crate::core::saved_queries::SavedQuery,
    params: HashMap<String, String>,
    state: Arc<ServerState>,
    start_time: std::time::Instant,
    ops_load_ms: f64,
) -> impl IntoResponse {
    match execute_read_operation(query, params, state, start_time, ops_load_ms).await {
        Ok(json) => (StatusCode::OK, Json(json)),
        Err((code, json)) => (code, Json(json)),
    }
}

async fn handle_batch(
    batch: &crate::core::saved_queries::SavedBatch,
    params: HashMap<String, String>,
    state: Arc<ServerState>,
) -> impl IntoResponse {
    let mut results = serde_json::Map::new();
    
    // Use cache
    let ops = {
        let mut cache = state.ops_cache.write().unwrap();
        let needs_reload = match &*cache {
            Some((ts, _)) => ts.elapsed().as_secs() > 5,
            None => true,
        };
        if needs_reload {
            let loaded = load_operations().unwrap_or_default();
            *cache = Some((std::time::Instant::now(), loaded.clone()));
            loaded
        } else {
            cache.as_ref().unwrap().1.clone()
        }
    };
    
    let mut max_pages = 0;
    let mut total_items_sum = 0;
    let mut execution_time_sum = 0.0;

    // Ejecutar todas las operaciones del batch
    for op_name in &batch.operations {
        if let Some(op) = ops.iter().find(|o| o.name() == op_name) {
            match op {
                SavedOperation::Read(ref q) => {
                    // Soporte para parámetros específicos -> nombre_query:param
                    let mut targeted_params = params.clone();
                    let prefix = format!("{}:", op_name);
                    for (k, v) in &params {
                        if let Some(stripped) = k.strip_prefix(&prefix) {
                            targeted_params.insert(stripped.to_string(), v.clone());
                        }
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
                            results.insert(op_name.clone(), serde_json::json!({
                                "error": "Query failed",
                                "status": code.as_u16(),
                                "details": res
                            }));
                        }
                    }
                },
                _ => {
                    results.insert(op_name.clone(), serde_json::json!({
                        "error": "Only Read operations are currently supported in Batch"
                    }));
                }
            }
        } else {
             results.insert(op_name.clone(), serde_json::json!({
                 "error": "Operation not found"
             }));
        }
    }

    let response = serde_json::json!({
        "results": results,
        "batch_meta": {
            "max_pages": max_pages,
            "total_items_combined": total_items_sum,
            "total_engine_time_ms": execution_time_sum,
            "queries_count": batch.operations.len()
        }
    });

    (StatusCode::OK, Json(response))
}

async fn execute_read_operation(
    query: &crate::core::saved_queries::SavedQuery,
    params: HashMap<String, String>,
    state: Arc<ServerState>,
    start_time: std::time::Instant,
    ops_load_ms: f64,
) -> Result<serde_json::Value, (StatusCode, serde_json::Value)> {
    let mut missing_params = Vec::new();

    // Convert SavedQuery to arguments for execute_query
    let filters: Vec<Filter> = query.filters.iter().map(|sf| {
        let mut val = sf.value.clone();
        if val.starts_with('$') {
            let key = &val[1..];
            if let Some(param_val) = params.get(key) {
                val = param_val.clone();
            } else {
                missing_params.push(key.to_string());
            }
        }
        Filter {
            field: sf.field.clone(),
            op: ComparisonOp::from_str(&sf.op),
            value: val,
            value_options: vec![],
        }
    }).collect();
    
    let mut aggregations = query.aggregations.clone();
    for agg in &mut aggregations {
        if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
            for val in obj.values_mut() {
                if let Some(s) = val.as_str() {
                    if let Some(key) = s.strip_prefix('$') {
                        if let Some(param_val) = params.get(key) {
                            if let Ok(num) = param_val.parse::<u64>() {
                                *val = serde_json::json!(num);
                            } else {
                                *val = serde_json::json!(param_val);
                            }
                        } else {
                            missing_params.push(key.to_string());
                        }
                    }
                }
            }
        }
    }

    if !missing_params.is_empty() {
         missing_params.sort();
         missing_params.dedup();
         return Err((StatusCode::BAD_REQUEST, serde_json::json!({
             "error": "Missing required query parameters",
             "missing_parameters": missing_params
         })));
    }

    let filters_op = match query.filters_op.as_str() {
        "Or" => LogicalOp::Or,
        _ => LogicalOp::And,
    };
    
    let order_by: Vec<OrderBy> = query.order_by.iter().map(|so| {
        OrderBy {
            field: so.field.clone(),
            direction: if so.direction == "Desc" { SortDirection::Desc } else { SortDirection::Asc }
        }
    }).collect();
    
    // Check for 'fields' override in params
    let param_fields: Vec<String> = params.get("fields")
        .map(|s| s.split(',').map(|f| f.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();

    let limit = if let Some(ref param) = query.limit_param {
        let key = param.strip_prefix('$').unwrap_or(param);
        params.get(key).and_then(|s| s.parse::<usize>().ok()).or(query.limit)
    } else {
        query.limit
    }.unwrap_or(100).min(100);
    let page = params.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
    let offset = (page - 1) * limit;

    let fields = if !param_fields.is_empty() {
        param_fields
    } else if query.selected_fields.is_empty() && query.aggregations.is_empty() {
        let all_fields = crate::repl::utils::get_indexed_fields(&query.entity, &query.table);
        crate::repl::utils::get_base_fields(&all_fields)
    } else {
        query.selected_fields.clone()
    };

    let query_entity_for_search = query.entity.clone();
    let query_table_for_search = query.table.clone();
    let state_clone_for_search = state.clone();
    let fields_for_search = fields.clone();

    let setup_ms = start_time.elapsed().as_secs_f64() * 1000.0;
    let engine_start = std::time::Instant::now();

    let result = tokio::task::spawn_blocking(move || {
        let t0 = std::time::Instant::now();
        let table_res = state_clone_for_search.table_manager.get_table(&query_entity_for_search, &query_table_for_search);
        let t1 = std::time::Instant::now();

        match table_res {
            Ok(table_lock) => {
                let table = table_lock.read().unwrap();
                let t2 = std::time::Instant::now();
                let mut search_res = table.search(&fields_for_search, &filters, &filters_op, &aggregations, &order_by, limit, offset)?;
                
                // Add server-side timing diagnostics to debug_info
                let open_ms = t1.duration_since(t0).as_secs_f64() * 1000.0;
                let lock_ms = t2.duration_since(t1).as_secs_f64() * 1000.0;
                let extra = format!(" | Open: {:.2}ms, Lock: {:.2}ms", open_ms, lock_ms);
                
                if let Some(ref mut d) = search_res.debug_info {
                    d.push_str(&extra);
                } else {
                    search_res.debug_info = Some(extra);
                }
                Ok(search_res)
            },
            Err(e) => Err(e)
        }
    }).await.unwrap();

    let engine_total_ms = engine_start.elapsed().as_secs_f64() * 1000.0;

    match result {
        Ok(query_result) => {
            let mapping_start = std::time::Instant::now();
            let headers = &query_result.headers;
            
            // --- LAZY LOADING OPTIMIZATION ---
            // If the motor returned row_ids but empty rows (lazy mode), fetch them now.
            // This is safer for memory as we don't hold two copies of the data as long.
            let rows = if query_result.rows.is_empty() && query_result.row_ids.is_some() {
                let ids = query_result.row_ids.as_ref().unwrap().clone();
                let entity_inner = query.entity.clone();
                let table_inner = query.table.clone();
                let fields_inner = fields.clone();
                let table_manager_inner = state.table_manager.clone();
                
                // Fetch all rows in one go for HTTP (as we must return a single JSON)
                let fetch_res: anyhow::Result<Vec<Vec<String>>> = tokio::task::spawn_blocking(move || {
                    let table_lock = table_manager_inner.get_table(&entity_inner, &table_inner).unwrap();
                    let table = table_lock.read().unwrap();
                    table.get_rows_batch(&fields_inner, &ids)
                }).await.unwrap();
                fetch_res.unwrap()
            } else {
                query_result.rows
            };
            // ---------------------------------

            let row_to_json = |headers: &[String], row: &Vec<String>| {
                let mut map = serde_json::Map::new();
                for (i, header) in headers.iter().enumerate() {
                    if let Some(val) = row.get(i) {
                        if val.is_empty() {
                            // No incluir claves con valor vacío (ahorra payload)
                            continue;
                        }
                        let first_char = if val.is_empty() { b'\0' } else { val.as_bytes()[0] };
                        let json_val = if first_char.is_ascii_digit() || first_char == b'-' {
                            if let Ok(n) = val.parse::<i64>() {
                                serde_json::Value::Number(n.into())
                            } else if let Ok(f) = val.parse::<f64>() {
                                serde_json::Number::from_f64(f).map(serde_json::Value::Number).unwrap_or(serde_json::Value::String(val.clone()))
                            } else {
                                serde_json::Value::String(val.clone())
                            }
                        } else {
                            serde_json::Value::String(val.clone())
                        };
                        map.insert(header.clone(), json_val);
                    }
                }
                map
            };

            let data: Vec<serde_json::Map<String, serde_json::Value>> = rows.into_par_iter().map(|row| {
                row_to_json(headers, &row)
            }).collect();

            let aggregations_data: Option<Vec<serde_json::Value>> = query_result.aggregations.map(|aggs| {
                aggs.into_iter().map(|agg| {
                    let agg_headers = agg.headers;
                    let rows_json: Vec<serde_json::Map<String, serde_json::Value>> = agg.rows.into_iter().map(|row| {
                        row_to_json(&agg_headers, &row)
                    }).collect();
                    
                    serde_json::json!({
                        "data": rows_json,
                        "summary": agg.summary
                    })
                }).collect()
            });

            let mapping_ms = mapping_start.elapsed().as_secs_f64() * 1000.0;
            let engine_time_ms = query_result.execution_time_micros as f64 / 1000.0;
            
            let mut response_map = serde_json::Map::new();
            
            // Siempre incluir data, aunque esté vacía
            response_map.insert("data".to_string(), serde_json::json!(data));
            
            if let Some(aggs) = aggregations_data {
                response_map.insert("aggregations".to_string(), serde_json::json!(aggs));
            }
            
            let total_server_ms = start_time.elapsed().as_secs_f64() * 1000.0;

            response_map.insert("meta".to_string(), serde_json::json!({
                "engine_time_ms": engine_time_ms,
                "engine_total_ms": engine_total_ms,
                "ops_load_ms": ops_load_ms,
                "setup_ms": setup_ms,
                "mapping_ms": mapping_ms,
                "total_server_ms": total_server_ms,
                "total_found": query_result.total_found,
                "fields_count": headers.len(),
                "debug_info": query_result.debug_info
            }));
            
            // Solo incluir paginación si hay registros totales Y NO es solo una agregación
            // O si hay data presente
            if query_result.total_found > 0 && !headers.is_empty() {
                let total_pages = if limit > 0 { (query_result.total_found + limit - 1) / limit } else { 1 };
                response_map.insert("pagination".to_string(), serde_json::json!({
                    "page": page,
                    "per_page": limit,
                    "total_pages": total_pages.max(1),
                    "total_items": query_result.total_found
                }));
            }

            Ok(serde_json::Value::Object(response_map))
        },
        Err(e) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "error": e.to_string() })))
        }
    }
}


async fn handle_insert(
    def: &crate::core::saved_queries::SavedInsert,
    payload: HashMap<String, String>,
    state: Arc<ServerState>,
) -> impl IntoResponse {
    // Validate required fields if defined in CLI
    if !def.expected_fields.is_empty() {
        let mut missing = Vec::new();
        for f in &def.expected_fields {
            if !payload.contains_key(f) { missing.push(f.clone()); }
        }
        if !missing.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "error": "Missing required fields in body",
                "missing_fields": missing
            })));
        }
    }

    let result = match state.table_manager.get_table(&def.entity, &def.table) {
        Ok(table_lock) => {
            let mut table = table_lock.write().unwrap();
            table.insert(payload)
        },
        Err(e) => Err(e)
    };

    match result {
        Ok(_) => {
            let _ = state.log_sender.send(format!("  -> 201 Created in {}/{}", def.entity, def.table)).await;
            (StatusCode::CREATED, Json(serde_json::json!({ "status": "success", "message": "Record inserted" })))
        },
        Err(e) => {
            let _ = state.log_sender.send(format!("  -> 500 Error: {}", e)).await;
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))
        }
    }
}
