use tonic::{Request, Response, Status};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use std::sync::Arc;
use crate::server::table_manager::TableManager;
use crate::core::types::{Filter as CoreFilter, ComparisonOp, LogicalOp, SortDirection, OrderBy as CoreOrderBy};
// Table import removed

// Importar el código generado por tonic
pub mod bittice_proto {
    tonic::include_proto!("bittice");
}

use bittice_proto::database_server::{Database, DatabaseServer};
use bittice_proto::{
    SearchRequest, SearchResponse, SearchUnaryResponse, 
    Row as ProtoRow, AggregationResult as ProtoAggregationResult,
    search_response::Content, SearchMetadata, RowBatch
};

const BATCH_SIZE: usize = 1000;

pub struct MyDatabase {
    table_manager: Arc<TableManager>,
}

impl MyDatabase {
    pub fn new(table_manager: Arc<TableManager>) -> Self {
        Self { table_manager }
    }
}

use crate::core::saved_queries::load_operations;

#[tonic::async_trait]
impl Database for MyDatabase {
    type SearchStream = ReceiverStream<Result<SearchResponse, Status>>;
    type ExecuteSavedQueryStream = ReceiverStream<Result<SearchResponse, Status>>;

    async fn execute_saved_query(
        &self,
        request: Request<bittice_proto::SavedQueryRequest>,
    ) -> Result<Response<Self::ExecuteSavedQueryStream>, Status> {
        let req = request.into_inner();
        let table_manager = self.table_manager.clone();
        
        // Cargar query guardada
        let ops = load_operations().unwrap_or_default();
        let operation = ops.into_iter().find(|o| o.name() == req.query_name);

        match operation {
            Some(crate::core::saved_queries::SavedOperation::Read(query)) => {
                let (tx, rx) = mpsc::channel(100);

                tokio::spawn(async move {
                    // 1. Reemplazar parámetros ($var)
                    let params_map = req.params;
                    let mut missing_params = Vec::new();

                    let filters: Vec<CoreFilter> = query.filters.iter().map(|sf| {
                        let mut val = sf.value.clone();
                        if val.starts_with('$') {
                            let key = &val[1..];
                            if let Some(param_val) = params_map.get(key) {
                                val = param_val.clone();
                            } else {
                                missing_params.push(key.to_string());
                            }
                        }
                        CoreFilter {
                            field: sf.field.clone(),
                            op: ComparisonOp::from_str(&sf.op),
                            value: val,
                            value_options: vec![],
                        }
                    }).collect();

                    // Reemplazar params en agregaciones
                    let mut aggregations = query.aggregations.clone();
                    for agg in &mut aggregations {
                        if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                            for val in obj.values_mut() {
                                if let Some(s) = val.as_str() {
                                    if let Some(key) = s.strip_prefix('$') {
                                        if let Some(param_val) = params_map.get(key) {
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

                    if let Some(ref param) = query.limit_param {
                        let key = param.strip_prefix('$').unwrap_or(param);
                        if params_map.get(key).is_none() {
                            missing_params.push(key.to_string());
                        }
                    }
                    if !missing_params.is_empty() {
                        let _ = tx.send(Err(Status::invalid_argument(format!("Missing params: {:?}", missing_params)))).await;
                        return;
                    }
                    
                    let filters_op = match query.filters_op.as_str() {
                        "Or" => LogicalOp::Or,
                        _ => LogicalOp::And,
                    };

                    let order_by: Vec<CoreOrderBy> = query.order_by.iter().map(|so| {
                        CoreOrderBy {
                            field: so.field.clone(),
                            direction: if so.direction == "Desc" { SortDirection::Desc } else { SortDirection::Asc }
                        }
                    }).collect();

                    let mut limit = if let Some(ref param) = query.limit_param {
                        let key = param.strip_prefix('$').unwrap_or(param);
                        params_map.get(key).and_then(|s| s.parse::<usize>().ok()).or(query.limit)
                    } else {
                        query.limit
                    }.unwrap_or(100).max(1);
                    if req.limit_override > 0 {
                        limit = req.limit_override as usize;
                    }
                    let offset = req.offset_override as usize; // Default 0 if not set

                    let fields = if query.selected_fields.is_empty() && query.aggregations.is_empty() {
                         crate::repl::utils::get_indexed_fields(std::path::Path::new("data"), &query.entity, &query.table)
                    } else {
                        query.selected_fields.clone()
                    };

                    let entity = query.entity.clone();
                    let table_name = query.table.clone();
                    
                    // Ejecutar búsqueda (blocking)
                    let result = tokio::task::spawn_blocking(move || {
                        match table_manager.get_table(&entity, &table_name) {
                            Ok(table_lock) => {
                                let table = table_lock.read().unwrap();
                                table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, offset)
                            },
                            Err(e) => Err(e)
                        }
                    }).await.unwrap();

                    match result {
                        Ok(query_result) => {
                             // Reuse logic: Send Metadata
                            let meta = SearchMetadata {
                                headers: query_result.headers.clone(),
                                total_found: query_result.total_found as u64,
                                execution_time_micros: query_result.execution_time_micros as u64,
                                debug_info: query_result.debug_info.unwrap_or_default(),
                            };
                            if let Err(_) = tx.send(Ok(SearchResponse {
                                content: Some(Content::Metadata(meta))
                            })).await { return; }

                            // Send Rows in Batches
                            let mut buffer = Vec::with_capacity(BATCH_SIZE);
                            for row in query_result.rows {
                                buffer.push(ProtoRow { values: row });
                                if buffer.len() >= BATCH_SIZE {
                                    if let Err(_) = tx.send(Ok(SearchResponse {
                                        content: Some(Content::Rows(RowBatch { rows: buffer }))
                                    })).await { return; }
                                    buffer = Vec::with_capacity(BATCH_SIZE);
                                }
                            }
                            if !buffer.is_empty() {
                                if let Err(_) = tx.send(Ok(SearchResponse {
                                    content: Some(Content::Rows(RowBatch { rows: buffer }))
                                })).await { return; }
                            }


                            // Send Aggregations
                            if let Some(aggs) = query_result.aggregations {
                                for agg in aggs {
                                    let proto_agg = ProtoAggregationResult {
                                        headers: agg.headers,
                                        rows: agg.rows.into_iter().map(|r| ProtoRow { values: r }).collect(),
                                        summary: agg.summary.unwrap_or(0.0),
                                    };
                                    if let Err(_) = tx.send(Ok(SearchResponse {
                                        content: Some(Content::Aggregation(proto_agg))
                                    })).await { return; }
                                }
                            }
                        },
                        Err(e) => {
                            let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                        }
                    }
                });

                Ok(Response::new(ReceiverStream::new(rx)))
            },
            Some(_) => Err(Status::unimplemented("Only READ operations are supported via gRPC for now")),
            None => Err(Status::not_found(format!("Query '{}' not found", req.query_name))),
        }
    }

    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        let req = request.into_inner();
        let table_manager = self.table_manager.clone();

        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(async move {
            let filters_op_enum = req.filters_op(); // Capture before move
            let entity = req.entity;
            let table_name = req.table;

            // Validación básica
            if entity.is_empty() || table_name.is_empty() {
                let _ = tx.send(Err(Status::invalid_argument("Entity and Table are required"))).await;
                return;
            }

            // Mapeo de argumentos
            let filters: Vec<CoreFilter> = req.filters.iter().map(|f| {
                CoreFilter {
                    field: f.field.clone(),
                    op: match f.op() {
                        bittice_proto::ComparisonOp::Eq => ComparisonOp::Eq,
                        bittice_proto::ComparisonOp::Ne => ComparisonOp::Ne,
                        bittice_proto::ComparisonOp::Gt => ComparisonOp::Gt,
                        bittice_proto::ComparisonOp::Gte => ComparisonOp::Gte,
                        bittice_proto::ComparisonOp::Lt => ComparisonOp::Lt,
                        bittice_proto::ComparisonOp::Lte => ComparisonOp::Lte,
                        bittice_proto::ComparisonOp::Like => ComparisonOp::Like,
                        bittice_proto::ComparisonOp::In => ComparisonOp::In,
                    },
                    value: f.value.clone(),
                    value_options: f.value_options.clone(),
                }
            }).collect();

            let filters_op = match filters_op_enum {
                bittice_proto::LogicalOp::Or => LogicalOp::Or,
                _ => LogicalOp::And,
            };

            let order_by: Vec<CoreOrderBy> = req.order_by.iter().map(|o| {
                CoreOrderBy {
                    field: o.field.clone(),
                    direction: match o.direction() {
                        bittice_proto::SortDirection::Desc => SortDirection::Desc,
                        _ => SortDirection::Asc,
                    }
                }
            }).collect();

            // Parsear agregaciones desde JSON
            let mut aggregations = Vec::new();
            for agg_req in req.aggregations {
                if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&agg_req.aggregation_json) {
                    aggregations.push(json_val);
                }
            }

            let limit = req.limit as usize;
            let offset = req.offset as usize;
            
            // Determinar campos
            let fields = if req.selected_fields.is_empty() && aggregations.is_empty() {
                crate::repl::utils::get_indexed_fields(std::path::Path::new("data"), &entity, &table_name)
            } else {
                req.selected_fields.clone()
            };

            // Ejecutar búsqueda (blocking)
            let result = tokio::task::spawn_blocking(move || {
                match table_manager.get_table(&entity, &table_name) {
                    Ok(table_lock) => {
                        let table = table_lock.read().unwrap();
                        table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, offset)
                    },
                    Err(e) => Err(e)
                }
            }).await.unwrap();

            match result {
                Ok(query_result) => {
                    // 1. Enviar Metadata
                    let meta = SearchMetadata {
                        headers: query_result.headers.clone(),
                        total_found: query_result.total_found as u64,
                        execution_time_micros: query_result.execution_time_micros as u64,
                        debug_info: query_result.debug_info.unwrap_or_default(),
                    };
                    if let Err(_) = tx.send(Ok(SearchResponse {
                        content: Some(Content::Metadata(meta))
                    })).await {
                        return; // Cliente desconectado
                    }

                    // 2. Enviar Rows en Batches
                    let mut buffer = Vec::with_capacity(BATCH_SIZE);
                    for row in query_result.rows {
                        buffer.push(ProtoRow { values: row });
                        if buffer.len() >= BATCH_SIZE {
                            if let Err(_) = tx.send(Ok(SearchResponse {
                                content: Some(Content::Rows(RowBatch { rows: buffer }))
                            })).await {
                                return;
                            }
                            buffer = Vec::with_capacity(BATCH_SIZE);
                        }
                    }
                    if !buffer.is_empty() {
                         if let Err(_) = tx.send(Ok(SearchResponse {
                            content: Some(Content::Rows(RowBatch { rows: buffer }))
                        })).await {
                            return;
                        }
                    }


                    // 3. Enviar Agregaciones (si hay)
                    if let Some(aggs) = query_result.aggregations {
                        for agg in aggs {
                            let proto_agg = ProtoAggregationResult {
                                headers: agg.headers,
                                rows: agg.rows.into_iter().map(|r| ProtoRow { values: r }).collect(),
                                summary: agg.summary.unwrap_or(0.0),
                            };
                            if let Err(_) = tx.send(Ok(SearchResponse {
                                content: Some(Content::Aggregation(proto_agg))
                            })).await {
                                return;
                            }
                        }
                    }
                },
                Err(e) => {
                    let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn search_unary(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<SearchUnaryResponse>, Status> {
        let req = request.into_inner();
        let table_manager = self.table_manager.clone();
        
        let filters_op_enum = req.filters_op(); // Capture before move
        let entity = req.entity;
        let table_name = req.table;
        
         if entity.is_empty() || table_name.is_empty() {
             return Err(Status::invalid_argument("Entity and Table are required"));
         }

        let filters: Vec<CoreFilter> = req.filters.iter().map(|f| {
            CoreFilter {
                field: f.field.clone(),
                op: match f.op() {
                    bittice_proto::ComparisonOp::Eq => ComparisonOp::Eq,
                    bittice_proto::ComparisonOp::Ne => ComparisonOp::Ne,
                    bittice_proto::ComparisonOp::Gt => ComparisonOp::Gt,
                    bittice_proto::ComparisonOp::Gte => ComparisonOp::Gte,
                    bittice_proto::ComparisonOp::Lt => ComparisonOp::Lt,
                    bittice_proto::ComparisonOp::Lte => ComparisonOp::Lte,
                    bittice_proto::ComparisonOp::Like => ComparisonOp::Like,
                    bittice_proto::ComparisonOp::In => ComparisonOp::In,
                },
                value: f.value.clone(),
                value_options: f.value_options.clone(),
            }
        }).collect();

        let filters_op = match filters_op_enum {
            bittice_proto::LogicalOp::Or => LogicalOp::Or,
            _ => LogicalOp::And,
        };

        let order_by: Vec<CoreOrderBy> = req.order_by.iter().map(|o| {
            CoreOrderBy {
                field: o.field.clone(),
                direction: match o.direction() {
                    bittice_proto::SortDirection::Desc => SortDirection::Desc,
                    _ => SortDirection::Asc,
                }
            }
        }).collect();

        let mut aggregations = Vec::new();
        for agg_req in req.aggregations {
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&agg_req.aggregation_json) {
                aggregations.push(json_val);
            }
        }

        let limit = req.limit as usize;
        let offset = req.offset as usize;

        let fields = if req.selected_fields.is_empty() && aggregations.is_empty() {
            crate::repl::utils::get_indexed_fields(std::path::Path::new("data"), &entity, &table_name)
        } else {
            req.selected_fields.clone()
        };

        let result = tokio::task::spawn_blocking(move || {
            match table_manager.get_table(&entity, &table_name) {
                Ok(table_lock) => {
                    let table = table_lock.read().unwrap();
                    table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, offset)
                },
                Err(e) => Err(e)
            }
        }).await.unwrap();

        match result {
            Ok(query_result) => {
                let proto_rows: Vec<ProtoRow> = query_result.rows.into_iter()
                    .map(|r| ProtoRow { values: r })
                    .collect();
                
                let proto_aggs: Vec<ProtoAggregationResult> = query_result.aggregations.unwrap_or_default().into_iter()
                    .map(|agg| ProtoAggregationResult {
                        headers: agg.headers,
                        rows: agg.rows.into_iter().map(|r| ProtoRow { values: r }).collect(),
                        summary: agg.summary.unwrap_or(0.0),
                    }).collect();

                Ok(Response::new(SearchUnaryResponse {
                    headers: query_result.headers,
                    rows: proto_rows,
                    total_found: query_result.total_found as u64,
                    execution_time_micros: query_result.execution_time_micros as u64,
                    debug_info: query_result.debug_info.unwrap_or_default(),
                    aggregations: proto_aggs,
                }))
            },
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }
}

// Helper to start the server
pub async fn start_grpc_server(port: u16) -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{}", port).parse()?;
    let table_manager = Arc::new(TableManager::new());
    let db = MyDatabase::new(table_manager);
    
    println!("gRPC Server listening on {}", addr);

    tonic::transport::Server::builder()
        .add_service(DatabaseServer::new(db))
        .serve(addr)
        .await?;
        
    Ok(())
}
