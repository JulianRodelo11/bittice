use axum::{
    debug_handler,
    extract::{Path, State, Query},
    response::Json,
    routing::get,
    Router,
};
use std::sync::{Arc};
use tokio::sync::{mpsc, oneshot};
use tower_http::trace::TraceLayer;
use crate::core::saved_queries::{load_queries};
use crate::core::storage::table::Table;
use crate::core::types::{Filter, LogicalOp, ComparisonOp, SortDirection, OrderBy};
use std::collections::HashMap;

// Estructura para compartir estado con los handlers de Axum
struct ServerState {
    log_sender: mpsc::Sender<String>,
}

pub async fn start_server(log_sender: mpsc::Sender<String>, shutdown_rx: oneshot::Receiver<()>) {
    let state = Arc::new(ServerState {
        log_sender: log_sender.clone(),
    });

    // Definir rutas
    let app = Router::new()
        .route("/*path", get(handle_query))
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
async fn handle_query(
    Path(path): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<ServerState>>,
) -> Json<serde_json::Value> {
    let _ = state.log_sender.send(format!("GET /{}", path)).await;
    
    // Cargar queries (en cada petición para hot-reload simple)
    let queries = load_queries().unwrap_or_default();
    
    // Buscar la query por nombre (path)
    // Normalizamos el path para quitar posibles slash iniciales extra si el framework los deja
    let query_name = path.trim_start_matches('/');
    
    if let Some(query) = queries.iter().find(|q| q.name == query_name || q.name == path) {
        // Ejecutar query
        
        let mut missing_params = Vec::new();

        // Convertir SavedQuery a argumentos para execute_query
        let filters: Vec<Filter> = query.filters.iter().map(|sf| {
            let mut val = sf.value.clone();
            if val.starts_with('$') {
                // Remove $ for the parameter key
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
                value_options: vec![], // No necesario para ejecución
            }
        }).collect();
        
        let mut aggregations = query.aggregations.clone();
        for agg in &mut aggregations {
            if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                for val in obj.values_mut() {
                    if let Some(s) = val.as_str() {
                        if let Some(key) = s.strip_prefix('$') {
                            if let Some(param_val) = params.get(key) {
                                // Try to parse as number for parameters like 'n' or 'limit'
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
             let _ = state.log_sender.send(format!("  -> 400 Bad Request (Missing params: {:?})", missing_params)).await;
             return Json(serde_json::json!({
                 "error": "Missing required query parameters",
                 "missing_parameters": missing_params
             }));
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
        
        // Determine Limit and Offset
        // STRICT: Limit comes ONLY from the saved query definition.
        let limit = query.limit.unwrap_or(100).max(1);
        let page = params.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
        let offset = (page - 1) * limit;

         let fields = if query.selected_fields.is_empty() {
             crate::repl::utils::get_indexed_fields(std::path::Path::new("data"), &query.entity, &query.table)
         } else {
             query.selected_fields.clone()
         };

        let result = {
            let base_path = std::path::Path::new("data").join(&query.entity);
            match Table::open(&base_path, &query.table) {
                Ok(mut table) => table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, offset),
                Err(e) => Err(e)
            }
        };

        match result {
            Ok(result) => {
                let _ = state.log_sender.send(format!("  -> 200 OK (Found {})", result.total_found)).await;
                
                // Transform rows into standard JSON objects
                let data: Vec<serde_json::Map<String, serde_json::Value>> = result.rows.into_iter().map(|row| {
                    let mut map = serde_json::Map::new();
                    for (i, header) in result.headers.iter().enumerate() {
                        if let Some(val) = row.get(i) {
                            // If value is empty, return null
                            if val.is_empty() {
                                map.insert(header.clone(), serde_json::Value::Null);
                                continue;
                            }

                            // Try to parse numbers if possible, otherwise string
                            let json_val = if let Ok(n) = val.parse::<i64>() {
                                serde_json::Value::Number(n.into())
                            } else if let Ok(f) = val.parse::<f64>() {
                                serde_json::Number::from_f64(f)
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::String(val.clone()))
                            } else {
                                serde_json::Value::String(val.clone())
                            };
                            map.insert(header.clone(), json_val);
                        }
                    }
                    map
                }).collect();

                let actual_total = result.total_found;
                let actual_limit = limit;

                // Construct enhanced JSON response
                let total_pages = if actual_limit > 0 { (actual_total + actual_limit - 1) / actual_limit } else { 1 };
                let response = serde_json::json!({
                    "data": data,
                    "meta": {
                        "execution_time_ms": result.execution_time_micros as f64 / 1000.0,
                        "query": query_name,
                        "limit": actual_limit
                    },
                    "pagination": {
                        "page": page,
                        "per_page": actual_limit,
                        "total_pages": total_pages.max(1),
                        "total_items": actual_total
                    }
                });

                Json(response)
            },
            Err(e) => {
                let _ = state.log_sender.send(format!("  -> 500 Error: {}", e)).await;
                Json(serde_json::json!({
                    "error": e.to_string()
                }))
            }
        }
    } else {
        let _ = state.log_sender.send(format!("  -> 404 Not Found (Query '{}' not found)", query_name)).await;
        Json(serde_json::json!({
            "error": "Query not found. Make sure to save a query with this name first."
        }))
    }
}
