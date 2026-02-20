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
use crate::core::storage::table::Table;
use crate::core::types::{Filter, LogicalOp, ComparisonOp, SortDirection, OrderBy};
use std::collections::HashMap;
use rayon::prelude::*;

// Manejador de tablas para mantenerlas abiertas en memoria
struct TableManager {
    tables: RwLock<HashMap<String, Arc<RwLock<Table>>>>,
}

impl TableManager {
    fn new() -> Self {
        Self {
            tables: RwLock::new(HashMap::new()),
        }
    }

    fn get_table(&self, entity: &str, table_name: &str) -> anyhow::Result<Arc<RwLock<Table>>> {
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

// Estructura para compartir estado con los handlers de Axum
struct ServerState {
    log_sender: mpsc::Sender<String>,
    table_manager: TableManager,
}

pub async fn start_server(log_sender: mpsc::Sender<String>, shutdown_rx: oneshot::Receiver<()>) {
    let state = Arc::new(ServerState {
        log_sender: log_sender.clone(),
        table_manager: TableManager::new(),
    });

    // Definir rutas: Catch-all para cualquier método
    let app = Router::new()
        .route("/*path", any(handle_request))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = "127.0.0.1:3000";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let _ = log_sender.send(format!("Server started on http://{}", addr)).await;
    
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
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query_params: HashMap<String, String> = Query::try_from_uri(req.uri())
        .map(|Query(params)| params)
        .unwrap_or_default();
    
    let op_name = path.trim_start_matches('/');
    let _ = state.log_sender.send(format!("{} /{}", method, op_name)).await;

    // Load operations from CLI definitions
    let ops = load_operations().unwrap_or_default();
    
    // Find operation by name
    let operation = ops.iter().find(|o| o.name() == op_name);

    if let Some(op) = operation {
        match (method, op) {
            (Method::GET, SavedOperation::Read(q)) => {
                handle_read(q, query_params, state).await.into_response()
            },
            (Method::POST, SavedOperation::Insert(i)) => {
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
) -> impl IntoResponse {
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
         return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
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

    let limit = query.limit.unwrap_or(100).max(1);
    let page = params.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
    let offset = (page - 1) * limit;

    let fields = if !param_fields.is_empty() {
        param_fields
    } else if query.selected_fields.is_empty() && query.aggregations.is_empty() {
        crate::repl::utils::get_indexed_fields(std::path::Path::new("data"), &query.entity, &query.table)
    } else {
        query.selected_fields.clone()
    };

    let query_entity = query.entity.clone();
    let query_table = query.table.clone();
    let state_clone = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        match state_clone.table_manager.get_table(&query_entity, &query_table) {
            Ok(table_lock) => {
                let mut table = table_lock.write().unwrap();
                table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, offset)
            },
            Err(e) => Err(e)
        }
    }).await.unwrap();

    match result {
        Ok(result) => {
            let headers = &result.headers;
            let rows = result.rows;
            
            let row_to_json = |headers: &[String], row: &[String]| {
                let mut map = serde_json::Map::new();
                for (i, header) in headers.iter().enumerate() {
                    if let Some(val) = row.get(i) {
                        if val.is_empty() {
                            map.insert(header.clone(), serde_json::Value::Null);
                        } else {
                            let first_char = val.as_bytes()[0];
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
                }
                map
            };

            let data: Vec<serde_json::Map<String, serde_json::Value>> = rows.into_par_iter().map(|row| {
                row_to_json(headers, &row)
            }).collect();

            let aggregations_data: Option<Vec<serde_json::Value>> = result.aggregations.map(|aggs| {
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

            let total_pages = if limit > 0 { (result.total_found + limit - 1) / limit } else { 1 };
            let engine_time_ms = result.execution_time_micros as f64 / 1000.0;
            
            let mut response_map = serde_json::Map::new();
            
            if !data.is_empty() {
                response_map.insert("data".to_string(), serde_json::json!(data));
            }
            
            if let Some(aggs) = aggregations_data {
                response_map.insert("aggregations".to_string(), serde_json::json!(aggs));
            }
            
            response_map.insert("meta".to_string(), serde_json::json!({
                "engine_time_ms": engine_time_ms,
                "total_found": result.total_found,
                "fields_count": headers.len(),
                "debug_info": result.debug_info
            }));
            
            response_map.insert("pagination".to_string(), serde_json::json!({
                "page": page,
                "per_page": limit,
                "total_pages": total_pages.max(1),
                "total_items": result.total_found
            }));

            (StatusCode::OK, Json(serde_json::Value::Object(response_map)))
        },
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))
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
