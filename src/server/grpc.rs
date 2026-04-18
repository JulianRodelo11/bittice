use bittice_proto::database_server::DatabaseServer;
use tonic::{Request, Response, Status};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use std::sync::Arc;
use std::time::Instant;
use crate::server::table_manager::TableManager;
use crate::core::storage::table::Table;
use crate::core::types::{Filter as CoreFilter, ComparisonOp, LogicalOp, SortDirection, OrderBy as CoreOrderBy, FieldType, QueryResult};
use crate::core::saved_queries::{SavedOperation, SavedQuery};
use std::collections::HashMap;
use tracing::{info, debug, warn};

fn query_uses_rest_only_aggregations(query: &SavedQuery) -> Result<bool, String> {
    for aggregation in &query.aggregations {
        match crate::core::saved_queries::SavedCollectAggregation::from_aggregation(aggregation) {
            Ok(Some(_)) => return Ok(true),
            Ok(None) => {}
            Err(error) => return Err(format!("Invalid Collect aggregation: {}", error)),
        }
    }

    Ok(false)
}

pub mod bittice_proto {
    tonic::include_proto!("bittice");
}

use bittice_proto::database_server::{Database};
use bittice_proto::{
    SearchRequest, SearchResponse, SearchUnaryResponse, 
    Row as ProtoRow, AggregationResult as ProtoAggregationResult,
    search_response::Content, SearchMetadata, RowBatch,
    ComparisonOp as ProtoComparisonOp, LogicalOp as ProtoLogicalOp,
    SortDirection as ProtoSortDirection, FieldType as ProtoFieldType,
    SavedQueryRequest
};

fn proto_comparison_op_to_str(op: i32) -> String {
    match ProtoComparisonOp::try_from(op).unwrap_or(ProtoComparisonOp::Eq) {
        ProtoComparisonOp::Eq => "Eq",
        ProtoComparisonOp::Ne => "Ne",
        ProtoComparisonOp::Gt => "Gt",
        ProtoComparisonOp::Gte => "Gte",
        ProtoComparisonOp::Lt => "Lt",
        ProtoComparisonOp::Lte => "Lte",
        ProtoComparisonOp::In => "In",
    }.to_string()
}

fn proto_logical_op_to_str(op: i32) -> String {
    match ProtoLogicalOp::try_from(op).unwrap_or(ProtoLogicalOp::And) {
        ProtoLogicalOp::And => "And",
        ProtoLogicalOp::Or => "Or",
    }.to_string()
}

fn proto_sort_direction_to_str(dir: i32) -> String {
    match ProtoSortDirection::try_from(dir).unwrap_or(ProtoSortDirection::Asc) {
        ProtoSortDirection::Asc => "Asc",
        ProtoSortDirection::Desc => "Desc",
    }.to_string()
}

fn proto_field_type_to_core(ft: i32) -> Option<FieldType> {
    match ProtoFieldType::try_from(ft).ok() {
        Some(ProtoFieldType::String) => Some(FieldType::String),
        Some(ProtoFieldType::Int) => Some(FieldType::Int),
        Some(ProtoFieldType::Float) => Some(FieldType::Float),
        Some(ProtoFieldType::Date) => Some(FieldType::Date),
        _ => None,
    }
}

const BATCH_SIZE: usize = 1000;

pub struct MyDatabase {
    table_manager: Arc<TableManager>,
    ops_cache: Arc<RwLock<Option<(Instant, Arc<Vec<SavedOperation>>)>>>,
    entity_filter: Option<String>,
    auth_service: crate::core::auth::AuthService,
}

impl MyDatabase {
    pub fn new(table_manager: Arc<TableManager>, entity_filter: Option<String>) -> Self {
        Self { 
            table_manager: table_manager.clone(),
            ops_cache: Arc::new(RwLock::new(None)),
            entity_filter,
            auth_service: crate::core::auth::AuthService::new(table_manager),
        }
    }

    async fn extract_auth_context(&self, metadata: &tonic::metadata::MetadataMap, config: Option<&crate::core::saved_queries::SavedAuthConfig>) -> Option<crate::core::types::AuthContext> {
        let token = metadata.get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        
        if let Some(t) = token {
            let entity = self.entity_filter.clone().unwrap_or_else(|| "default".to_string());
            self.auth_service.resolve_token(&entity, t, config).await
        } else {
            None
        }
    }

    async fn get_operations(&self) -> Arc<Vec<SavedOperation>> {
        {
            let cache = self.ops_cache.read().await;
            if let Some((ts, ops)) = &*cache {
                if ts.elapsed().as_secs() < 60 {
                    return Arc::clone(ops);
                }
            }
        }
        let mut cache = self.ops_cache.write().await;
        let loaded = crate::core::saved_queries::load_operations_with_filter(self.entity_filter.clone()).unwrap_or_default();
        let ops_arc = Arc::new(loaded);
        *cache = Some((Instant::now(), Arc::clone(&ops_arc)));
        ops_arc
    }
}

async fn execute_query_unary_internal(
    query: SavedQuery,
    params_map: HashMap<String, String>,
    table_manager: Arc<TableManager>,
    limit_override: u32,
    offset_override: u32,
    auth_context: Option<crate::core::types::AuthContext>,
) -> Result<SearchUnaryResponse, Status> {
    match execute_query_result_internal(
        query,
        params_map,
        table_manager,
        limit_override,
        offset_override,
        auth_context,
    ).await {
        Ok(query_result) => {
            let mut proto_rows = Vec::new();
            for row in query_result.rows {
                proto_rows.push(ProtoRow { values: row });
            }

            let mut proto_aggs = Vec::new();
            if let Some(aggs) = query_result.aggregations {
                for agg in aggs {
                    let mut rows = Vec::new();
                    for r in agg.rows { rows.push(ProtoRow { values: r }); }
                    proto_aggs.push(ProtoAggregationResult {
                        headers: agg.headers,
                        rows,
                        summary: agg.summary.unwrap_or(0.0),
                    });
                }
            }

            Ok(SearchUnaryResponse {
                headers: query_result.headers,
                rows: proto_rows,
                total_found: query_result.total_found as u64,
                execution_time_micros: query_result.execution_time_micros as u64,
                debug_info: "".to_string(),
                aggregations: proto_aggs,
                pagination: None,
            })
        }
        Err(status) => Err(status),
    }
}

async fn execute_query_result_internal(
    query: SavedQuery,
    params_map: HashMap<String, String>,
    table_manager: Arc<TableManager>,
    limit_override: u32,
    offset_override: u32,
    auth_context: Option<crate::core::types::AuthContext>,
) -> Result<QueryResult, Status> {
    let entity = query.entity.clone();
    let table_name = query.table.clone();

    let filters: Vec<CoreFilter> = query.filters.iter().map(|sf| {
        let mut val = sf.value.clone();
        if val.starts_with('$') {
            if let Some(param_val) = params_map.get(&val[1..]) { val = param_val.clone(); }
        }
        CoreFilter {
            field: sf.field.clone(),
            op: ComparisonOp::from_str(&sf.op),
            value: val,
            value_to: sf.value_to.as_ref().map(|raw| {
                if let Some(key) = raw.strip_prefix('$') {
                    params_map.get(key).cloned().unwrap_or_else(|| raw.clone())
                } else {
                    raw.clone()
                }
            }),
            value_options: sf.values.iter().map(|raw| {
                if let Some(key) = raw.strip_prefix('$') {
                    params_map.get(key).cloned().unwrap_or_else(|| raw.clone())
                } else {
                    raw.clone()
                }
            }).collect(),
            field_type: sf.field_type,
        }
    }).collect();

    let mut aggregations = query.aggregations.clone();
    for agg in &mut aggregations {
        if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
            for val in obj.values_mut() {
                if let Some(s) = val.as_str() {
                    if let Some(key) = s.strip_prefix('$') {
                        if let Some(param_val) = params_map.get(key) {
                            if let Ok(num) = param_val.parse::<u64>() { *val = serde_json::json!(num); }
                            else { *val = serde_json::json!(param_val); }
                        }
                    }
                }
            }
        }
    }

    let filters_op = match query.filters_op.as_str() { "Or" => LogicalOp::Or, _ => LogicalOp::And };
    let order_by: Vec<CoreOrderBy> = query.order_by.iter().map(|so| {
        CoreOrderBy { field: so.field.clone(), direction: if so.direction == "Desc" { SortDirection::Desc } else { SortDirection::Asc } }
    }).collect();

    let mut limit = if let Some(ref param) = query.limit_param {
        let key = param.strip_prefix('$').unwrap_or(param);
        params_map.get(key).and_then(|s| s.parse::<usize>().ok()).or(query.limit)
    } else { query.limit }.unwrap_or(100).min(100);

    if limit_override > 0 { limit = limit_override as usize; }

    let page = params_map.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
    let mut offset = (page - 1) * limit;
    if offset_override > 0 { offset = offset_override as usize; }

    if query.is_multi_table() {
        let query_clone = query.clone();
        let params_clone = params_map.clone();
        let auth_clone = auth_context.clone();
        return tokio::task::spawn_blocking(move || {
            crate::core::join_query::execute_join_query(
                &query_clone,
                &params_clone,
                table_manager,
                None,
                limit,
                offset,
                auth_clone.as_ref(),
            )
        })
        .await
        .unwrap()
        .map_err(|e| Status::internal(e.to_string()));
    }

    tokio::task::spawn_blocking(move || {
        if let Ok(table_arc) = table_manager.get_table(&entity, &table_name) {
            let mut table = table_arc.write().unwrap();
            let _ = table.reload_if_needed();

            let mut f_search = query.selected_fields.clone();
            if f_search.iter().any(|f| f == "*") {
                let mut all_cols = table.manifest.original_fields.clone();
                if all_cols.is_empty() {
                    all_cols = Table::get_indexed_fields_static(&entity, &table_name);
                    all_cols.retain(|f| !f.ends_with("_day") && !f.ends_with("_month") && !f.ends_with("_hour_bucket"));
                }
                let mut new_f = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for f in f_search {
                    if f == "*" {
                        for c in &all_cols { if seen.insert(c.clone()) { new_f.push(c.clone()); } }
                    } else if seen.insert(f.clone()) {
                        new_f.push(f);
                    }
                }
                f_search = new_f;
            }

            table.search(&f_search, &filters, &filters_op, &aggregations, &order_by, limit, offset, auth_context.as_ref())
        } else {
            Err(anyhow::anyhow!("Table not found"))
        }
    })
    .await
    .unwrap()
    .map_err(|e| Status::internal(e.to_string()))
}

#[tonic::async_trait]
impl Database for MyDatabase {
    type SearchStream = ReceiverStream<Result<SearchResponse, Status>>;

    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        let metadata = request.metadata().clone();
        let auth_ctx = self.extract_auth_context(&metadata, None).await;
        let req = request.into_inner();
        let entity = req.entity.clone();
        let table_name = req.table.clone();
        
        let mut filters = Vec::new();
        for f in req.filters {
            filters.push(CoreFilter {
                field: f.field,
                op: ComparisonOp::from_str(&proto_comparison_op_to_str(f.op)),
                value: f.value,
                value_to: None,
                value_options: vec![],
                field_type: proto_field_type_to_core(f.field_type),
            });
        }

        let filters_op = if proto_logical_op_to_str(req.filters_op) == "Or" { LogicalOp::Or } else { LogicalOp::And };
        let mut order_by = Vec::new();
        for o in req.order_by {
            order_by.push(CoreOrderBy {
                field: o.field,
                direction: if proto_sort_direction_to_str(o.direction) == "Desc" { SortDirection::Desc } else { SortDirection::Asc },
            });
        }

        let limit = if req.limit > 0 { req.limit as usize } else { 100 };
        let offset = req.offset as usize;
        let fields = req.selected_fields;

        let table_manager = Arc::clone(&self.table_manager);
        let (tx, rx) = mpsc::channel(10);

        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || {
                if let Ok(table_arc) = table_manager.get_table(&entity, &table_name) {
                    let mut table = table_arc.write().unwrap();
                    let _ = table.reload_if_needed();
                    table.search(&fields, &filters, &filters_op, &[], &order_by, limit, offset, auth_ctx.as_ref())
                } else {
                    Err(anyhow::anyhow!("Table not found"))
                }
            }).await.unwrap();

            match res {
                Ok(query_result) => {
                    // Send metadata first
                    let metadata = SearchMetadata {
                        headers: query_result.headers.clone(),
                        total_found: query_result.total_found as u64,
                        execution_time_micros: query_result.execution_time_micros as u64,
                        debug_info: "".to_string(),
                        pagination: None,
                    };
                    if let Err(_) = tx.send(Ok(SearchResponse {
                        content: Some(Content::Metadata(metadata)),
                    })).await { return; }

                    // Then send rows in chunks
                    for chunk in query_result.rows.chunks(BATCH_SIZE) {
                        let mut proto_rows = Vec::new();
                        for row in chunk {
                            proto_rows.push(ProtoRow { values: row.clone() });
                        }
                        let resp = SearchResponse {
                            content: Some(Content::Rows(RowBatch { rows: proto_rows })),
                        };
                        if let Err(_) = tx.send(Ok(resp)).await { break; }
                    }
                }
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
        let metadata = request.metadata().clone();
        let auth_ctx = self.extract_auth_context(&metadata, None).await;
        let req = request.into_inner();
        
        let query = SavedQuery {
            name: "ad-hoc".to_string(),
            entity: req.entity,
            table: req.table,
            table_alias: None,
            joins: Vec::new(),
            filters: req.filters.into_iter().map(|f| crate::core::saved_queries::SavedFilter {
                field: f.field,
                op: proto_comparison_op_to_str(f.op),
                value: f.value,
                value_to: None,
                values: Vec::new(),
                field_type: proto_field_type_to_core(f.field_type),
            }).collect(),
            filter_tree: None,
            filters_op: proto_logical_op_to_str(req.filters_op),
            aggregations: Vec::new(),
            order_by: req.order_by.into_iter().map(|o| crate::core::saved_queries::SavedOrderBy {
                field: o.field,
                direction: proto_sort_direction_to_str(o.direction),
            }).collect(),
            limit: Some(req.limit as usize),
            limit_param: None,
            selected_fields: req.selected_fields,
            select: Vec::new(),
            response_grouping: None,
            auth_config: None,
        };

        let resp = execute_query_unary_internal(
            query, 
            HashMap::new(), 
            Arc::clone(&self.table_manager),
            req.limit,
            req.offset,
            auth_ctx,
        ).await?;

        Ok(Response::new(resp))
    }

    type ExecuteSavedQueryStream = ReceiverStream<Result<SearchResponse, Status>>;

    async fn execute_saved_query(
        &self,
        request: Request<SavedQueryRequest>,
    ) -> Result<Response<Self::ExecuteSavedQueryStream>, Status> {
        let metadata = request.metadata().clone();
        let req = request.into_inner();
        let ops = self.get_operations().await;
        
        let query = if let Some(op) = ops.iter().find(|o| o.name() == req.query_name) {
            if let SavedOperation::Read(q) = op {
                if q.response_grouping.is_some() {
                    return Err(Status::invalid_argument("response_grouping is currently supported only by the REST API"));
                }
                if query_uses_rest_only_aggregations(q).map_err(Status::invalid_argument)? {
                    return Err(Status::invalid_argument("Collect aggregation is currently supported only by the REST API"));
                }
                q.clone()
            } else {
                return Err(Status::invalid_argument("Not a read operation"));
            }
        } else {
            return Err(Status::not_found("Query not found"));
        };

        let auth_ctx = self.extract_auth_context(&metadata, query.auth_config.as_ref()).await;

        let table_manager = Arc::clone(&self.table_manager);
        let (tx, rx) = mpsc::channel(10);

        tokio::spawn(async move {
            let res = execute_query_result_internal(
                query,
                req.params,
                table_manager,
                req.limit_override,
                req.offset_override,
                auth_ctx,
            ).await;

            match res {
                Ok(query_result) => {
                    let metadata = SearchMetadata {
                        headers: query_result.headers.clone(),
                        total_found: query_result.total_found as u64,
                        execution_time_micros: query_result.execution_time_micros as u64,
                        debug_info: "".to_string(),
                        pagination: None,
                    };
                    let _ = tx.send(Ok(SearchResponse {
                        content: Some(Content::Metadata(metadata)),
                    })).await;

                    for chunk in query_result.rows.chunks(BATCH_SIZE) {
                        let mut proto_rows = Vec::new();
                        for row in chunk {
                            proto_rows.push(ProtoRow { values: row.clone() });
                        }
                        let _ = tx.send(Ok(SearchResponse {
                            content: Some(Content::Rows(RowBatch { rows: proto_rows })),
                        })).await;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn execute_saved_query_unary(
        &self,
        request: Request<SavedQueryRequest>,
    ) -> Result<Response<SearchUnaryResponse>, Status> {
        let metadata = request.metadata().clone();
        let req = request.into_inner();
        let ops = self.get_operations().await;
        
        if let Some(op) = ops.iter().find(|o| o.name() == req.query_name) {
            if let SavedOperation::Read(q) = op {
                if q.response_grouping.is_some() {
                    return Err(Status::invalid_argument("response_grouping is currently supported only by the REST API"));
                }
                if query_uses_rest_only_aggregations(q).map_err(Status::invalid_argument)? {
                    return Err(Status::invalid_argument("Collect aggregation is currently supported only by the REST API"));
                }
                let auth_ctx = self.extract_auth_context(&metadata, q.auth_config.as_ref()).await;
                let resp = execute_query_unary_internal(
                    q.clone(), 
                    req.params, 
                    Arc::clone(&self.table_manager),
                    req.limit_override,
                    req.offset_override,
                    auth_ctx,
                ).await?;
                return Ok(Response::new(resp));
            }
        }
        
        Err(Status::not_found("Query not found"))
    }

    type SubscribeUpdatesStream = ReceiverStream<Result<bittice_proto::UpdateEvent, Status>>;

    async fn subscribe_updates(
        &self,
        request: Request<bittice_proto::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeUpdatesStream>, Status> {
        let metadata = request.metadata().clone();
        let req = request.into_inner();
        let query_name = req.query_name.trim().to_string();
        let params = req.params.clone();
        
        if query_name.is_empty() { return Err(Status::invalid_argument("Query name required")); }

        // Use cached and filtered operations
        let ops = self.get_operations().await;
        
        let (entity, table_name, filters, auth_cfg) = if let Some(op) = ops.iter().find(|o| o.name() == query_name) {
            if let SavedOperation::Read(q) = op {
                if q.is_multi_table() {
                    return Err(Status::invalid_argument("SubscribeUpdates does not support multi-table queries yet"));
                }
                let mut resolved_filters = Vec::new();
                for sf in &q.filters {
                    let mut val = sf.value.clone();
                    if val.starts_with('$') {
                        if let Some(p_val) = params.get(&val[1..]) { val = p_val.clone(); }
                    }
                    resolved_filters.push(CoreFilter {
                        field: sf.field.clone(),
                        op: ComparisonOp::from_str(&sf.op),
                        value: val,
                        value_to: sf.value_to.clone(),
                        value_options: sf.values.clone(),
                        field_type: sf.field_type,
                    });
                }
                (q.entity.clone(), q.table.clone(), resolved_filters, q.auth_config.clone())
            } else { return Err(Status::not_found("Query found but it is not a 'read' operation")); }
        } else { 
            return Err(Status::not_found(format!("Query name '{}' not found in current configuration", query_name))); 
        };

        // Re-resolve auth context with query-specific configuration
        let auth_ctx = self.extract_auth_context(&metadata, auth_cfg.as_ref()).await;

        let table_manager = Arc::clone(&self.table_manager);
        let mut events_rx = table_manager.events_tx.subscribe();
        let (tx, rx) = mpsc::channel(100);

        // Get table columns for mapping
        let columns = if let Ok(table_arc) = table_manager.get_table(&entity, &table_name) {
            let table = table_arc.read().unwrap();
            table.manifest.original_fields.clone()
        } else {
            vec![]
        };

        tokio::spawn(async move {
            let entity_filter = entity.to_lowercase();
            let table_filter = table_name.to_lowercase();
            let mut final_filters = filters;
            
            // Inject identity filter for subscription
            if let Some(ctx) = auth_ctx {
                let filter_col = ctx.filter_col.clone();
                final_filters.push(CoreFilter {
                    field: filter_col,
                    op: ComparisonOp::Eq,
                    value: ctx.user_id,
                    value_to: None,
                    value_options: vec![],
                    field_type: None,
                });
            }
            
            let filters_internal = final_filters;
            let cols_internal = if columns.is_empty() {
                if let Ok(table_arc) = table_manager.get_table(&entity, &table_name) {
                    table_arc.read().unwrap().manifest.original_fields.clone()
                } else { vec![] }
            } else { columns };

            debug!("gRPC: Client subscribed to '{}' (Entity: {}, Table: {})", 
                query_name, entity_filter, table_filter);

            loop {
                match events_rx.recv().await {
                    Ok(event) => {
                        let e_name = event.entity.trim().to_lowercase();
                        let t_name = event.table_name.trim().to_lowercase();

                        if e_name == entity_filter && t_name == table_filter {
                            let mut is_match = true;
                            
                            if !filters_internal.is_empty() && !cols_internal.is_empty() && !event.row.is_empty() {
                                let mut row_data: HashMap<&String, &String> = HashMap::new();
                                for (i, col_name) in cols_internal.iter().enumerate() {
                                    if i < event.row.len() { 
                                        row_data.insert(col_name, &event.row[i]); 
                                    }
                                }

                                for f in &filters_internal {
                                    if let Some(actual_val) = row_data.get(&f.field) {
                                        let actual_trimmed = actual_val.trim();
                                        let filter_trimmed = f.value.trim();
                                        
                                        let matched = crate::core::types::compare_filter_value(
                                            actual_trimmed,
                                            f.op,
                                            filter_trimmed,
                                            f.value_to.as_deref(),
                                            &f.value_options,
                                            f.field_type,
                                        );

                                        if !matched {
                                            is_match = false;
                                            break;
                                        }
                                    } else {
                                        is_match = false; 
                                        break;
                                    }
                                }
                            } else if !filters_internal.is_empty() {
                                is_match = false;
                            }

                            if is_match {
                                debug!("gRPC [{}]: Notification sent for {}/{} (PK: {})", query_name, e_name, t_name, event.pk);
                                let proto_event = bittice_proto::UpdateEvent {
                                    r#type: event.event_type.clone(),
                                    table: event.table_name.clone(),
                                    row: Some(bittice_proto::Row { values: event.row.clone() }),
                                    pk: event.pk.clone(),
                                };
                                if let Err(_) = tx.send(Ok(proto_event)).await { break; }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("gRPC [{}]: Stream lagged, skipped {} events", query_name, skipped);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub async fn start_grpc_server(port: u16, entity_filter: Option<String>) -> anyhow::Result<()> {
    let table_manager = Arc::new(TableManager::new());
    start_grpc_server_with_manager(port, table_manager, entity_filter, None).await
}

pub async fn start_grpc_server_with_manager(port: u16, table_manager: Arc<TableManager>, entity_filter: Option<String>, _log_tx: Option<mpsc::Sender<String>>) -> anyhow::Result<()> {
    let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let addr = format!("{}:{}", host, port).parse()?;
    
    let db = MyDatabase::new(table_manager, entity_filter);
    
    info!("gRPC Server listening on {}", addr);

    tonic::transport::Server::builder()
        .add_service(DatabaseServer::new(db))
        .serve(addr)
        .await?;

    Ok(())
}
