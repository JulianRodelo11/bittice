use tonic::{Request, Response, Status};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use std::sync::Arc;
use std::time::Instant;
use crate::server::table_manager::TableManager;
use crate::core::types::{Filter as CoreFilter, ComparisonOp, LogicalOp, SortDirection, OrderBy as CoreOrderBy, QueryResult};
use crate::core::saved_queries::{load_operations, SavedOperation, SavedQuery};
use std::collections::HashMap;

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
    ops_cache: Arc<RwLock<Option<(Instant, Arc<Vec<SavedOperation>>)>>>,
}

impl MyDatabase {
    pub fn new(table_manager: Arc<TableManager>) -> Self {
        Self { 
            table_manager,
            ops_cache: Arc::new(RwLock::new(None)),
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
        let loaded = load_operations().unwrap_or_default();
        let ops_arc = Arc::new(loaded);
        *cache = Some((Instant::now(), Arc::clone(&ops_arc)));
        ops_arc
    }
}

async fn execute_query_internal(
    query: SavedQuery,
    params_map: HashMap<String, String>,
    table_manager: Arc<TableManager>,
    tx: mpsc::Sender<Result<SearchResponse, Status>>,
    limit_override: u32,
    offset_override: u32,
) -> Result<(), Status> {
    let mut missing_params = Vec::new();
    let entity = query.entity.clone();
    let table_name = query.table.clone();

    let filters: Vec<CoreFilter> = query.filters.iter().map(|sf| {
        let mut val = sf.value.clone();
        if val.starts_with('$') {
            let key = &val[1..];
            if let Some(param_val) = params_map.get(key) { val = param_val.clone(); }
            else { missing_params.push(key.to_string()); }
        }
        CoreFilter { field: sf.field.clone(), op: ComparisonOp::from_str(&sf.op), value: val, value_options: vec![] }
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
                        } else { missing_params.push(key.to_string()); }
                    }
                }
            }
        }
    }

    if let Some(ref param) = query.limit_param {
        let key = param.strip_prefix('$').unwrap_or(param);
        if params_map.get(key).is_none() && limit_override == 0 { missing_params.push(key.to_string()); }
    }

    if !missing_params.is_empty() {
        let _ = tx.send(Err(Status::invalid_argument(format!("Missing params for {}: {:?}", query.name, missing_params)))).await;
        return Ok(());
    }

    let filters_op = match query.filters_op.as_str() { "Or" => LogicalOp::Or, _ => LogicalOp::And };
    let order_by: Vec<CoreOrderBy> = query.order_by.iter().map(|so| {
        CoreOrderBy { field: so.field.clone(), direction: if so.direction == "Desc" { SortDirection::Desc } else { SortDirection::Asc } }
    }).collect();

    let mut limit = if let Some(ref param) = query.limit_param {
        let key = param.strip_prefix('$').unwrap_or(param);
        params_map.get(key).and_then(|s| s.parse::<usize>().ok()).or(query.limit)
    } else { query.limit }.unwrap_or(100).min(100);
    
    if limit == 0 { limit = 100; }
    if limit_override > 0 { limit = (limit_override as usize).min(100); }
    let offset = offset_override as usize;

    let fields = if query.selected_fields.is_empty() && query.aggregations.is_empty() {
         let all_fields = crate::repl::utils::get_indexed_fields(&entity, &table_name);
         crate::repl::utils::get_base_fields(&all_fields)
    } else { query.selected_fields.clone() };

    let table_manager_inner = table_manager.clone();
    let fields_inner = fields.clone();
    let entity_inner = entity.clone();
    let table_name_inner = table_name.clone();

    let result: anyhow::Result<QueryResult> = tokio::task::spawn_blocking(move || {
        let t_open_start = Instant::now();
        let table_lock = table_manager_inner.get_table(&entity_inner, &table_name_inner)?;
        let t_open_ms = t_open_start.elapsed().as_millis();
        
        {
            let mut table = table_lock.write().unwrap();
            let _ = table.reload_if_needed();
        }

        let table = table_lock.read().unwrap();
        let mut search_res = table.search(&fields_inner, &filters, &filters_op, &aggregations, &order_by, limit, offset)?;
        
        let extra_debug = format!(" | OpenTable: {}ms", t_open_ms);
        if let Some(ref mut d) = search_res.debug_info { d.push_str(&extra_debug); }
        else { search_res.debug_info = Some(extra_debug); }
        
        Ok(search_res)
    }).await.unwrap();

    match result {
        Ok(query_result) => {
            let total_found = query_result.total_found as u64;
            let page = if limit > 0 { (offset as u32 / limit as u32) + 1 } else { 1 };
            let total_pages = if limit > 0 { (total_found as u32 + limit as u32 - 1) / limit as u32 } else { 1 };

            let meta = SearchMetadata {
                headers: query_result.headers.clone(),
                total_found,
                execution_time_micros: query_result.execution_time_micros as u64,
                debug_info: query_result.debug_info.unwrap_or_default(),
                pagination: Some(bittice_proto::PaginationMetadata {
                    page,
                    per_page: limit as u32,
                    total_pages,
                    total_items: total_found,
                }),
            };
            if let Err(_) = tx.send(Ok(SearchResponse { content: Some(Content::Metadata(meta)) })).await { return Ok(()); }

            if let Some(row_ids) = query_result.row_ids {
                for chunk in row_ids.chunks(BATCH_SIZE) {
                    let tm_chunk = table_manager.clone();
                    let f_chunk = fields.clone();
                    let ids_chunk = chunk.to_vec();
                    let e_chunk = entity.clone();
                    let t_chunk = table_name.clone();

                    let rows_res: anyhow::Result<Vec<Vec<String>>> = tokio::task::spawn_blocking(move || {
                        let table_lock = tm_chunk.get_table(&e_chunk, &t_chunk)?;
                        let table = table_lock.read().unwrap();
                        table.get_rows_batch(&f_chunk, &ids_chunk)
                    }).await.unwrap();

                    match rows_res {
                        Ok(rows) => {
                            let proto_rows = rows.into_iter().map(|r| ProtoRow { values: r }).collect();
                            if let Err(_) = tx.send(Ok(SearchResponse { content: Some(Content::Rows(RowBatch { rows: proto_rows })) })).await { return Ok(()); }
                        },
                        Err(e) => {
                            let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                            return Ok(());
                        }
                    }
                }
            }

            if let Some(aggs) = query_result.aggregations {
                for agg in aggs {
                    let proto_agg = ProtoAggregationResult {
                        headers: agg.headers,
                        rows: agg.rows.into_iter().map(|r| ProtoRow { values: r }).collect(),
                        summary: agg.summary.unwrap_or(0.0),
                    };
                    if let Err(_) = tx.send(Ok(SearchResponse { content: Some(Content::Aggregation(proto_agg)) })).await { return Ok(()); }
                }
            }
        },
        Err(e) => { let _ = tx.send(Err(Status::internal(e.to_string()))).await; }
    }
    Ok(())
}

#[tonic::async_trait]
impl Database for MyDatabase {
    type SearchStream = ReceiverStream<Result<SearchResponse, Status>>;
    type ExecuteSavedQueryStream = ReceiverStream<Result<SearchResponse, Status>>;
    type SubscribeUpdatesStream = ReceiverStream<Result<bittice_proto::UpdateEvent, Status>>;

    async fn execute_saved_query(
        &self,
        request: Request<bittice_proto::SavedQueryRequest>,
    ) -> Result<Response<Self::ExecuteSavedQueryStream>, Status> {
        let req = request.into_inner();
        let table_manager = Arc::clone(&self.table_manager);
        let (tx, rx) = mpsc::channel(100);

        let ops = self.get_operations().await;
        let operation = ops.iter().find(|o| o.name() == req.query_name).cloned();

        match operation {
            Some(SavedOperation::Read(query)) => {
                tokio::spawn(async move {
                    let _ = execute_query_internal(query, req.params, table_manager, tx, req.limit_override, req.offset_override).await;
                });
            },
            Some(SavedOperation::Batch(batch)) => {
                tokio::spawn(async move {
                    let mut handles = Vec::new();
                    for op_name in batch.operations {
                        if let Some(SavedOperation::Read(q)) = ops.iter().find(|o| o.name() == op_name).cloned() {
                            let mut targeted_params = req.params.clone();
                            let prefix = format!("{}:", op_name);
                            for (k, v) in &req.params {
                                if let Some(stripped) = k.strip_prefix(&prefix) { targeted_params.insert(stripped.to_string(), v.clone()); }
                            }
                            let tx_clone = tx.clone();
                            let tm_clone = table_manager.clone();
                            let lim = req.limit_override;
                            let off = req.offset_override;
                            
                            handles.push(tokio::spawn(async move {
                                let _ = execute_query_internal(q, targeted_params, tm_clone, tx_clone, lim, off).await;
                            }));
                        }
                    }
                    for h in handles { let _ = h.await; }
                });
            },
            Some(_) => return Err(Status::unimplemented("Only READ/BATCH supported")),
            None => return Err(Status::not_found(format!("Query '{}' not found", req.query_name))),
        }

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn search(&self, request: Request<SearchRequest>) -> Result<Response<Self::SearchStream>, Status> {
        let req = request.into_inner();
        let table_manager_outer = Arc::clone(&self.table_manager);
        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(async move {
            let filters_op_enum = req.filters_op();
            let entity = req.entity.clone();
            let table_name = req.table.clone();

            if entity.is_empty() || table_name.is_empty() {
                let _ = tx.send(Err(Status::invalid_argument("Entity/Table required"))).await;
                return;
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

            let filters_op = match filters_op_enum { bittice_proto::LogicalOp::Or => LogicalOp::Or, _ => LogicalOp::And };
            let order_by: Vec<CoreOrderBy> = req.order_by.iter().map(|o| {
                CoreOrderBy { field: o.field.clone(), direction: match o.direction() { bittice_proto::SortDirection::Desc => SortDirection::Desc, _ => SortDirection::Asc } }
            }).collect();

            let mut aggregations = Vec::new();
            for agg_req in req.aggregations {
                if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&agg_req.aggregation_json) { aggregations.push(json_val); }
            }

            let limit = if req.limit == 0 { 100 } else { (req.limit as usize).min(100) };
            let offset = req.offset as usize;
            let fields = if req.selected_fields.is_empty() && aggregations.is_empty() {
                let all_fields = crate::repl::utils::get_indexed_fields(&entity, &table_name);
                crate::repl::utils::get_base_fields(&all_fields)
            } else { req.selected_fields.clone() };

            let fields_for_search = fields.clone();
            let table_manager_for_search = Arc::clone(&table_manager_outer);
            let entity_for_search = entity.clone();
            let table_name_for_search = table_name.clone();

            let result: anyhow::Result<QueryResult> = tokio::task::spawn_blocking(move || {
                let table_lock = table_manager_for_search.get_table(&entity_for_search, &table_name_for_search)?;
                {
                    let mut table = table_lock.write().unwrap();
                    let _ = table.reload_if_needed();
                }
                let table = table_lock.read().unwrap();
                table.search(&fields_for_search, &filters, &filters_op, &aggregations, &order_by, limit, offset)
            }).await.unwrap();

            match result {
                Ok(query_result) => {
                    let total_found = query_result.total_found as u64;
                    let page_num = if limit > 0 { (offset as u32 / limit as u32) + 1 } else { 1 };
                    let total_pages = if limit > 0 { (total_found as u32 + limit as u32 - 1) / limit as u32 } else { 1 };

                    let meta = SearchMetadata {
                        headers: query_result.headers.clone(),
                        total_found,
                        execution_time_micros: query_result.execution_time_micros as u64,
                        debug_info: query_result.debug_info.unwrap_or_default(),
                        pagination: Some(bittice_proto::PaginationMetadata {
                            page: page_num,
                            per_page: limit as u32,
                            total_pages,
                            total_items: total_found,
                        }),
                    };
                    if let Err(_) = tx.send(Ok(SearchResponse { content: Some(Content::Metadata(meta)) })).await { return; }

                    if let Some(row_ids) = query_result.row_ids {
                        for chunk in row_ids.chunks(BATCH_SIZE) {
                            let tm_chunk = Arc::clone(&table_manager_outer);
                            let f_chunk = fields.clone();
                            let ids_chunk = chunk.to_vec();
                            let e_chunk = entity.clone();
                            let t_chunk = table_name.clone();

                            let rows_res: anyhow::Result<Vec<Vec<String>>> = tokio::task::spawn_blocking(move || {
                                let table_lock = tm_chunk.get_table(&e_chunk, &t_chunk)?;
                                let table = table_lock.read().unwrap();
                                table.get_rows_batch(&f_chunk, &ids_chunk)
                            }).await.unwrap();

                            match rows_res {
                                Ok(rows) => {
                                    let proto_rows = rows.into_iter().map(|r| ProtoRow { values: r }).collect();
                                    if let Err(_) = tx.send(Ok(SearchResponse { content: Some(Content::Rows(RowBatch { rows: proto_rows })) })).await { return; }
                                },
                                Err(e) => {
                                    let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                                    return;
                                }
                            }
                        }
                    }

                    if let Some(aggs) = query_result.aggregations {
                        for agg in aggs {
                            let proto_agg = ProtoAggregationResult {
                                headers: agg.headers,
                                rows: agg.rows.into_iter().map(|r| ProtoRow { values: r }).collect(),
                                summary: agg.summary.unwrap_or(0.0),
                            };
                            if let Err(_) = tx.send(Ok(SearchResponse { content: Some(Content::Aggregation(proto_agg)) })).await { return; }
                        }
                    }
                },
                Err(e) => { let _ = tx.send(Err(Status::internal(e.to_string()))).await; }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn search_unary(&self, request: Request<SearchRequest>) -> Result<Response<SearchUnaryResponse>, Status> {
        let req = request.into_inner();
        let table_manager = Arc::clone(&self.table_manager);
        let entity = req.entity.clone();
        let table_name = req.table.clone();
        
        if entity.is_empty() || table_name.is_empty() { return Err(Status::invalid_argument("Entity/Table required")); }

        let filters: Vec<CoreFilter> = req.filters.iter().map(|f| {
            CoreFilter { field: f.field.clone(), op: match f.op() { bittice_proto::ComparisonOp::Eq => ComparisonOp::Eq, bittice_proto::ComparisonOp::Ne => ComparisonOp::Ne, bittice_proto::ComparisonOp::Gt => ComparisonOp::Gt, bittice_proto::ComparisonOp::Gte => ComparisonOp::Gte, bittice_proto::ComparisonOp::Lt => ComparisonOp::Lt, bittice_proto::ComparisonOp::Lte => ComparisonOp::Lte, bittice_proto::ComparisonOp::Like => ComparisonOp::Like, bittice_proto::ComparisonOp::In => ComparisonOp::In, }, value: f.value.clone(), value_options: f.value_options.clone() }
        }).collect();

        let filters_op = match req.filters_op() { bittice_proto::LogicalOp::Or => LogicalOp::Or, _ => LogicalOp::And };
        let order_by: Vec<CoreOrderBy> = req.order_by.iter().map(|o| {
            CoreOrderBy { field: o.field.clone(), direction: match o.direction() { bittice_proto::SortDirection::Desc => SortDirection::Desc, _ => SortDirection::Asc } }
        }).collect();

        let mut aggregations = Vec::new();
        for agg_req in req.aggregations {
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&agg_req.aggregation_json) { aggregations.push(json_val); }
        }

        let limit = if req.limit == 0 { 100 } else { (req.limit as usize).min(100) };
        let offset = req.offset as usize;
        let fields = if req.selected_fields.is_empty() && aggregations.is_empty() {
            let all_fields = crate::repl::utils::get_indexed_fields(&entity, &table_name);
            crate::repl::utils::get_base_fields(&all_fields)
        } else { req.selected_fields.clone() };

        let result: anyhow::Result<QueryResult> = tokio::task::spawn_blocking(move || {
            let table_lock = table_manager.get_table(&entity, &table_name)?;
            {
                let mut table = table_lock.write().unwrap();
                let _ = table.reload_if_needed();
            }
            let table = table_lock.read().unwrap();
            table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, offset)
        }).await.unwrap();

        match result {
            Ok(query_result) => {
                let total_found = query_result.total_found as u64;
                let page_num = if limit > 0 { (offset as u32 / limit as u32) + 1 } else { 1 };
                let total_pages = if limit > 0 { (total_found as u32 + limit as u32 - 1) / limit as u32 } else { 1 };

                let proto_rows: Vec<ProtoRow> = query_result.rows.into_iter().map(|r| ProtoRow { values: r }).collect();
                let proto_aggs: Vec<ProtoAggregationResult> = query_result.aggregations.unwrap_or_default().into_iter().map(|agg| ProtoAggregationResult { headers: agg.headers, rows: agg.rows.into_iter().map(|r| ProtoRow { values: r }).collect(), summary: agg.summary.unwrap_or(0.0) }).collect();
                
                Ok(Response::new(SearchUnaryResponse { 
                    headers: query_result.headers, 
                    rows: proto_rows, 
                    total_found, 
                    execution_time_micros: query_result.execution_time_micros as u64, 
                    debug_info: query_result.debug_info.unwrap_or_default(), 
                    aggregations: proto_aggs,
                    pagination: Some(bittice_proto::PaginationMetadata {
                        page: page_num,
                        per_page: limit as u32,
                        total_pages,
                        total_items: total_found,
                    }),
                }))
            },
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn subscribe_updates(
        &self,
        request: Request<bittice_proto::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeUpdatesStream>, Status> {
        let req = request.into_inner();
        let entity = req.entity.trim().to_lowercase();
        let table_name = req.table.trim().to_lowercase();
        let table_manager = Arc::clone(&self.table_manager);
        let mut events_rx = table_manager.events_tx.subscribe();
        
        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(async move {
            let entity_filter = entity.clone();
            let table_filter = table_name.clone();

            println!("gRPC: Client subscribed to updates for {}/{}", entity_filter, table_filter);

            while let Ok(event) = events_rx.recv().await {
                let e_name = event.entity.trim().to_lowercase();
                let t_name = event.table_name.trim().to_lowercase();
                
                if e_name == entity_filter && t_name == table_filter {
                    println!("gRPC: Dispatching event to stream for client: {}/{} type={}", event.entity, event.table_name, event.event_type);
                    let proto_event = bittice_proto::UpdateEvent {
                        r#type: event.event_type.clone(),
                        table: event.table_name.clone(),
                        row: Some(bittice_proto::Row { values: event.row.clone() }),
                        pk: event.pk.clone(),
                    };
                    if let Err(e) = tx.send(Ok(proto_event)).await {
                        println!("gRPC: Client disconnected or channel closed: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub async fn start_grpc_server(port: u16) -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{}", port).parse()?;
    let table_manager = Arc::new(TableManager::new());
    
    let tm_watcher = Arc::clone(&table_manager);
    tokio::spawn(async move {
        let mut last_sequences: HashMap<String, u64> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            
            let data_dir = std::path::Path::new("data");
            if let Ok(entities) = std::fs::read_dir(data_dir) {
                for entity_entry in entities.flatten() {
                    if entity_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        let entity = entity_entry.file_name().to_string_lossy().to_string();
                        let entity_path = data_dir.join(&entity);
                        
                        if let Ok(tables) = std::fs::read_dir(entity_path) {
                            for table_entry in tables.flatten() {
                                if table_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                    let t_name = table_entry.file_name().to_string_lossy().to_string();
                                    let key = format!("{}/{}", entity, t_name);
                                    
                                    if let Ok(table_lock) = tm_watcher.get_table(&entity, &t_name) {
                                        let mut event_to_send = None;
                                        {
                                            let mut table = table_lock.write().unwrap();
                                            
                                            // Siempre intentamos recargar del disco
                                            let _ = table.reload_if_needed();
                                            let current_seq = table.manifest.last_sequence_number;
                                            
                                            if !last_sequences.contains_key(&key) {
                                                last_sequences.insert(key.clone(), current_seq);
                                                continue;
                                            }

                                            let old_seq = *last_sequences.get(&key).unwrap();
                                            
                                            if current_seq > old_seq {
                                                println!("Watcher: Change detected in {}/{} (seq {} -> {})", entity, t_name, old_seq, current_seq);
                                                let pk_field = table.manifest.primary_key.clone();
                                                
                                                let fields_to_fetch = if table.manifest.original_fields.is_empty() {
                                                    crate::repl::utils::get_indexed_fields(&entity, &t_name)
                                                } else {
                                                    table.manifest.original_fields.clone()
                                                };

                                                let mut row_data = vec![];
                                                
                                                if !fields_to_fetch.is_empty() {
                                                    // Intentar obtener la fila más reciente usando last_update
                                                    let mut order_by = vec![];
                                                    if fields_to_fetch.iter().any(|f| f == "last_update") {
                                                        order_by.push(CoreOrderBy {
                                                            field: "last_update".to_string(),
                                                            direction: SortDirection::Desc,
                                                        });
                                                    }

                                                    if let Ok(res) = table.search(&fields_to_fetch, &[], &LogicalOp::And, &[], &order_by, 1, 0) {
                                                        if let Some(r) = res.rows.into_iter().next() { row_data = r; }
                                                    }
                                                }

                                                event_to_send = Some(crate::server::table_manager::TableUpdateEvent {
                                                    entity: entity.clone(),
                                                    table_name: t_name.clone(),
                                                    event_type: "UPDATE".to_string(),
                                                    pk: pk_field,
                                                    row: row_data,
                                                });
                                                last_sequences.insert(key.clone(), current_seq);
                                            }
                                        }
                                        if let Some(ev) = event_to_send {
                                            println!("Watcher: Emitting broadcast event for {} (listeners: {})", key, tm_watcher.events_tx.receiver_count());
                                            let _ = tm_watcher.events_tx.send(ev);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let db = MyDatabase::new(table_manager);
    println!("gRPC Server listening on {}", addr);
    tonic::transport::Server::builder().add_service(DatabaseServer::new(db)).serve(addr).await?;
    Ok(())
}
