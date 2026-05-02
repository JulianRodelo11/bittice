use bittice_proto::database_server::DatabaseServer;
use tonic::{Request, Response, Status};
use tokio::sync::{mpsc, RwLock, Notify};
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

/// Join/subscribe helpers: `(alias, entity, physical_table)` for base table + each join.
fn subscribe_join_alias_map(q: &SavedQuery) -> Vec<(String, String, String)> {
    let ent = q.entity.trim().to_string();
    let base_alias = q
        .table_alias
        .clone()
        .unwrap_or_else(|| q.table.trim().to_string());
    let mut out = vec![(
        base_alias.clone(),
        ent.clone(),
        q.table.trim().to_string(),
    )];
    for j in &q.joins {
        let ja = j
            .alias
            .clone()
            .unwrap_or_else(|| j.table.trim().to_string());
        let je = j
            .entity
            .as_ref()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| ent.clone());
        out.push((ja, je, j.table.trim().to_string()));
    }
    out
}

fn subscribe_resolve_event_alias(
    alias_map: &[(String, String, String)],
    event_entity: &str,
    event_table: &str,
) -> Option<String> {
    let ee = event_entity.trim().to_lowercase();
    let et = event_table.trim().to_lowercase();
    for (alias, ent, tbl) in alias_map {
        if ent.trim().eq_ignore_ascii_case(&ee) && tbl.trim().eq_ignore_ascii_case(&et) {
            return Some(alias.clone());
        }
    }
    None
}

fn subscribe_entity_table_for_alias(
    alias_map: &[(String, String, String)],
    alias: &str,
) -> Option<(String, String)> {
    for (a, e, t) in alias_map {
        if a.eq_ignore_ascii_case(alias) {
            return Some((e.clone(), t.clone()));
        }
    }
    None
}

/// Keeps filters that apply to CDC rows from `event_alias`' table (`ev.col` → `col`).
/// Unqualified fields apply only when `event_alias` is the query base alias.
fn subscribe_filters_for_physical_row(
    filters: &[CoreFilter],
    event_alias: &str,
    base_alias: &str,
) -> Vec<CoreFilter> {
    let mut out = Vec::new();
    for f in filters {
        let col = match subscribe_normalize_filter_column(&f.field, event_alias, base_alias) {
            Some(c) => c,
            None => continue,
        };
        let mut nf = f.clone();
        nf.field = col;
        out.push(nf);
    }
    out
}

fn subscribe_normalize_filter_column(
    field: &str,
    event_alias: &str,
    base_alias: &str,
) -> Option<String> {
    let field = field.trim();
    if let Some(dot) = field.find('.') {
        let pref = field[..dot].trim();
        let col = field[dot + 1..].trim().to_string();
        if pref.eq_ignore_ascii_case(event_alias) {
            return Some(col);
        }
        return None;
    }
    if event_alias.eq_ignore_ascii_case(base_alias) {
        return Some(field.to_string());
    }
    None
}

fn subscribe_eval_filters_against_row(
    filters: &[CoreFilter],
    row_data: &HashMap<&String, &String>,
    filters_op: LogicalOp,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let eval_one = |f: &CoreFilter| {
        row_data
            .get(&f.field)
            .map(|actual_val| {
                crate::core::types::compare_filter_value(
                    actual_val.trim(),
                    f.op,
                    f.value.trim(),
                    f.value_to.as_deref(),
                    &f.value_options,
                    f.field_type,
                )
            })
            .unwrap_or(false)
    };
    match filters_op {
        LogicalOp::And => filters.iter().all(eval_one),
        LogicalOp::Or => filters.iter().any(eval_one),
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

    async fn execute_batch_unary_internal(
        &self,
        batch: &crate::core::saved_queries::SavedBatch,
        params: HashMap<String, String>,
        metadata: &tonic::metadata::MetadataMap,
    ) -> Result<SearchUnaryResponse, Status> {
        let mut results = HashMap::new();
        let ops = self.get_operations().await;
        
        let mut total_found: u64 = 0;
        let mut execution_time_micros: u64 = 0;

        for op_name in &batch.operations {
            if let Some(op) = ops.iter().find(|o| o.name() == op_name) {
                if let SavedOperation::Read(q) = op {
                    let mut targeted_params = params.clone();
                    let prefix = format!("{}:", op_name);
                    for (k, v) in &params {
                        if let Some(stripped) = k.strip_prefix(&prefix) {
                            targeted_params.insert(stripped.to_string(), v.clone());
                        }
                    }

                    let auth_ctx = self.extract_auth_context(metadata, q.auth_config.as_ref()).await;
                    match execute_query_result_internal(
                        q.clone(),
                        targeted_params,
                        Arc::clone(&self.table_manager),
                        0,
                        0,
                        auth_ctx
                    ).await {
                        Ok(res) => {
                            total_found = total_found.saturating_add(res.total_found as u64);
                            execution_time_micros = execution_time_micros.saturating_add(res.execution_time_micros as u64);
                            results.insert(op_name.clone(), res);
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }

        // Build computed fields
        let mut json_results = serde_json::Map::new();
        for (name, res) in &results {
            // Simplify QueryResult to JSON for expressions
            let mut rows_json = Vec::new();
            for row in &res.rows {
                let mut row_map = serde_json::Map::new();
                for (i, header) in res.headers.iter().enumerate() {
                    if let Some(val) = row.get(i) {
                        row_map.insert(header.clone(), serde_json::Value::String(val.clone()));
                    }
                }
                rows_json.push(serde_json::Value::Object(row_map));
            }
            
            let mut res_json = serde_json::json!({
                "headers": res.headers,
                "data": rows_json,
                "total_found": res.total_found,
            });

            if let Some(aggs) = &res.aggregations {
                if let Some(first) = aggs.first() {
                    res_json.as_object_mut().unwrap().insert("summary".to_string(), serde_json::json!(first.summary.unwrap_or(0.0)));
                }
            }

            json_results.insert(name.clone(), res_json);
        }

        let computed = self.build_batch_computed_fields_internal(&json_results, batch)?;

        // Handle response modes
        match batch.response_mode.as_deref() {
            Some("computed_only") => {
                let mut rows = Vec::new();
                let mut headers = Vec::new();
                let mut values = Vec::new();
                
                let mut sorted_keys: Vec<_> = computed.keys().collect();
                sorted_keys.sort();

                for key in sorted_keys {
                    headers.push(key.clone());
                    let val = computed.get(key).unwrap();
                    values.push(match val {
                        serde_json::Value::String(s) => s.clone(),
                        _ => val.to_string(),
                    });
                }
                rows.push(ProtoRow { values });

                Ok(SearchUnaryResponse {
                    headers,
                    rows,
                    total_found: 1,
                    execution_time_micros,
                    debug_info: "mode: computed_only".to_string(),
                    aggregations: Vec::new(),
                    pagination: None,
                })
            }
            Some("merge_first_data") => {
                let Some(first_op_name) = batch.operations.first() else {
                    return Err(Status::internal("Batch has no operations"));
                };
                let Some(source_res) = results.get(first_op_name) else {
                    return Err(Status::internal(format!("First operation '{}' result not found", first_op_name)));
                };

                let mut final_headers = source_res.headers.clone();
                let mut computed_headers: Vec<_> = computed.keys().collect();
                computed_headers.sort();
                
                for h in &computed_headers {
                    if !final_headers.contains(h) {
                        final_headers.push((*h).clone());
                    }
                }

                let mut final_rows = Vec::new();
                for row in &source_res.rows {
                    let mut values = row.clone();
                    // Pad if needed (though merge usually adds columns)
                    for h in &computed_headers {
                        let val = computed.get(*h).unwrap();
                        values.push(match val {
                            serde_json::Value::String(s) => s.clone(),
                            _ => val.to_string(),
                        });
                    }
                    final_rows.push(ProtoRow { values });
                }

                Ok(SearchUnaryResponse {
                    headers: final_headers,
                    rows: final_rows,
                    total_found: source_res.total_found as u64,
                    execution_time_micros,
                    debug_info: "mode: merge_first_data".to_string(),
                    aggregations: Vec::new(),
                    pagination: None,
                })
            }
            _ => {
                // Default: just return the first one for now or a combined view?
                // The proto SearchUnaryResponse doesn't easily support multiple results.
                // For now, let's return the first one with a warning.
                if let Some(first_op_name) = batch.operations.first() {
                    if let Some(source_res) = results.get(first_op_name) {
                         let mut proto_rows = Vec::new();
                         for row in &source_res.rows {
                             proto_rows.push(ProtoRow { values: row.clone() });
                         }
                         return Ok(SearchUnaryResponse {
                            headers: source_res.headers.clone(),
                            rows: proto_rows,
                            total_found: source_res.total_found as u64,
                            execution_time_micros,
                            debug_info: "default batch mode (first op only)".to_string(),
                            aggregations: Vec::new(),
                            pagination: None,
                        });
                    }
                }
                Err(Status::unimplemented("Generic batch response not yet implemented in gRPC"))
            }
        }
    }

    fn build_batch_computed_fields_internal(
        &self,
        results: &serde_json::Map<String, serde_json::Value>,
        batch: &crate::core::saved_queries::SavedBatch,
    ) -> Result<serde_json::Map<String, serde_json::Value>, Status> {
        let mut computed = serde_json::Map::new();
        for field in &batch.computed_fields {
            let mut context = evalexpr::HashMapContext::new();
            for (var_name, source) in &field.inputs {
                let value = self.resolve_batch_input_internal(results, source)
                    .map_err(|e| Status::internal(e))?;
                
                let eval_val = match value {
                    serde_json::Value::Number(n) => {
                        if let Some(f) = n.as_f64() { evalexpr::Value::Float(f) }
                        else if let Some(i) = n.as_i64() { evalexpr::Value::Int(i) }
                        else { evalexpr::Value::Float(0.0) }
                    }
                    serde_json::Value::String(s) => evalexpr::Value::String(s.clone()),
                    serde_json::Value::Bool(b) => evalexpr::Value::Boolean(b),
                    _ => evalexpr::Value::String(value.to_string()),
                };
                evalexpr::ContextWithMutableVariables::set_value(&mut context, var_name.clone().into(), eval_val).ok();
            }

            let result_val = evalexpr::eval_with_context(&field.expression, &context)
                .map_err(|e| Status::invalid_argument(format!("Error evaluating expression '{}': {}", field.expression, e)))?;
            
            computed.insert(field.name.clone(), match result_val {
                evalexpr::Value::Float(f) => serde_json::json!(f),
                evalexpr::Value::Int(i) => serde_json::json!(i),
                evalexpr::Value::Boolean(b) => serde_json::json!(b),
                evalexpr::Value::String(s) => serde_json::json!(s),
                _ => serde_json::json!(null),
            });
        }
        Ok(computed)
    }

    fn resolve_batch_input_internal(
        &self,
        results: &serde_json::Map<String, serde_json::Value>,
        source: &str,
    ) -> Result<serde_json::Value, String> {
        let parts: Vec<&str> = source.split('.').collect();
        if parts.len() < 2 { return Err(format!("Invalid input source: {}", source)); }

        let op_name = parts[0];
        let field_path = &parts[1..];

        let op_result = results.get(op_name)
            .ok_or_else(|| format!("Batch result '{}' not found", op_name))?;

        let mut current = op_result;
        for &part in field_path {
            if let Some(next) = current.get(part) {
                current = next;
            } else {
                return Ok(serde_json::Value::Number(serde_json::Number::from(0)));
            }
        }

        Ok(current.clone())
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
    fn param_key(raw: &str) -> Option<&str> {
        raw.strip_prefix('$')
            .and_then(|spec| spec.split('|').next())
            .map(str::trim)
            .filter(|key| !key.is_empty())
    }

    let entity = query.entity.clone();
    let table_name = query.table.clone();

    let filters: Vec<CoreFilter> = query.filters.iter().map(|sf| {
        let mut val = sf.value.clone();
        if let Some(key) = param_key(&val) {
            if let Some(param_val) = params_map.get(key) { val = param_val.clone(); }
        }
        CoreFilter {
            field: sf.field.clone(),
            op: ComparisonOp::from_str(&sf.op),
            value: val,
            value_to: sf.value_to.as_ref().map(|raw| {
                if let Some(key) = param_key(raw) {
                    params_map.get(key).cloned().unwrap_or_else(|| raw.clone())
                } else {
                    raw.clone()
                }
            }),
            value_options: sf.values.iter().map(|raw| {
                if let Some(key) = param_key(raw) {
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
                    if let Some(key) = param_key(s) {
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
        let key = param_key(param).unwrap_or(param);
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
            execution_profile: None,
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
            match op {
                SavedOperation::Read(q) => {
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
                SavedOperation::Batch(b) => {
                    let resp = self.execute_batch_unary_internal(
                        b,
                        req.params,
                        &metadata
                    ).await?;
                    return Ok(Response::new(resp));
                }
                _ => return Err(Status::invalid_argument(format!("Operation '{}' is not a read or batch operation and cannot be executed via this endpoint", req.query_name))),
            }
        }
        
        Err(Status::not_found(format!("Query '{}' not found", req.query_name)))
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

        if query_name.is_empty() {
            return Err(Status::invalid_argument("Query name required"));
        }

        let ops = self.get_operations().await;

        let (alias_map, base_alias, filters, auth_cfg, filters_op_logical) =
            if let Some(op) = ops.iter().find(|o| o.name() == query_name) {
                if let SavedOperation::Read(q) = op {
                    if q.response_grouping.is_some() {
                        return Err(Status::invalid_argument(
                            "SubscribeUpdates does not support response_grouping (REST-only)",
                        ));
                    }
                    if query_uses_rest_only_aggregations(q).map_err(Status::invalid_argument)? {
                        return Err(Status::invalid_argument(
                            "Collect aggregation is currently supported only by the REST API",
                        ));
                    }
                    let base_alias = q
                        .table_alias
                        .clone()
                        .unwrap_or_else(|| q.table.clone());
                    let alias_map = subscribe_join_alias_map(q);
                    let mut resolved_filters = Vec::new();
                    for sf in &q.filters {
                        let mut val = sf.value.clone();
                        if val.starts_with('$') {
                            if let Some(p_val) = params.get(&val[1..]) {
                                val = p_val.clone();
                            }
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
                    let filters_op_logical = match q.filters_op.as_str() {
                        "Or" => LogicalOp::Or,
                        _ => LogicalOp::And,
                    };
                    (
                        alias_map,
                        base_alias,
                        resolved_filters,
                        q.auth_config.clone(),
                        filters_op_logical,
                    )
                } else {
                    return Err(Status::not_found(
                        "Query found but it is not a 'read' operation",
                    ));
                }
            } else {
                return Err(Status::not_found(format!(
                    "Query name '{}' not found in current configuration",
                    query_name
                )));
            };

        let auth_ctx = self.extract_auth_context(&metadata, auth_cfg.as_ref()).await;

        let table_manager = Arc::clone(&self.table_manager);
        let mut events_rx = table_manager.events_tx.subscribe();
        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(async move {
            let mut final_filters = filters;

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

            let mut manifest_cols: HashMap<(String, String), Vec<String>> = HashMap::new();
            for (_, ent, tbl) in &alias_map {
                let key = (ent.trim().to_string(), tbl.trim().to_string());
                if manifest_cols.contains_key(&key) {
                    continue;
                }
                if let Ok(table_arc) = table_manager.get_table(&key.0, &key.1) {
                    manifest_cols.insert(
                        key.clone(),
                        table_arc.read().unwrap().manifest.original_fields.clone(),
                    );
                }
            }

            debug!(
                "gRPC: Client subscribed to '{}' (joined_tables={:?})",
                query_name,
                alias_map
                    .iter()
                    .map(|(a, e, t)| format!("{}:{}.{}", a, e, t))
                    .collect::<Vec<_>>()
            );

            loop {
                match events_rx.recv().await {
                    Ok(event) => {
                        let e_raw = event.entity.trim();
                        let t_raw = event.table_name.trim();

                        let Some(event_alias) =
                            subscribe_resolve_event_alias(&alias_map, e_raw, t_raw)
                        else {
                            continue;
                        };

                        let Some((phys_entity, phys_table)) =
                            subscribe_entity_table_for_alias(&alias_map, &event_alias)
                        else {
                            continue;
                        };

                        let phys_key = (
                            phys_entity.trim().to_string(),
                            phys_table.trim().to_string(),
                        );
                        let cols_internal = manifest_cols
                            .get(&phys_key)
                            .cloned()
                            .unwrap_or_default();

                        let filters_for_row = subscribe_filters_for_physical_row(
                            &filters_internal,
                            &event_alias,
                            &base_alias,
                        );

                        let pass = if filters_internal.is_empty() {
                            true
                        } else if filters_for_row.is_empty() {
                            // Filters reference only other aliases — skip events from this table.
                            continue;
                        } else if event.row.is_empty() {
                            // DELETE / minimal payloads cannot satisfy column filters (same as single-table path).
                            continue;
                        } else if cols_internal.is_empty() {
                            continue;
                        } else {
                            let mut row_data: HashMap<&String, &String> = HashMap::new();
                            for (i, col_name) in cols_internal.iter().enumerate() {
                                if i < event.row.len() {
                                    row_data.insert(col_name, &event.row[i]);
                                }
                            }
                            subscribe_eval_filters_against_row(
                                &filters_for_row,
                                &row_data,
                                filters_op_logical,
                            )
                        };

                        if pass {
                            debug!(
                                "gRPC [{}]: Notification {} {} alias={} PK={}",
                                query_name,
                                event.event_type,
                                event.table_name,
                                event_alias,
                                event.pk
                            );
                            let proto_event = bittice_proto::UpdateEvent {
                                r#type: event.event_type.clone(),
                                table: event.table_name.clone(),
                                row: Some(bittice_proto::Row {
                                    values: event.row.clone(),
                                }),
                                pk: event.pk.clone(),
                            };
                            if tx.send(Ok(proto_event)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            "gRPC [{}]: Stream lagged, skipped {} events",
                            query_name, skipped
                        );
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
    let never = Arc::new(Notify::new());
    start_grpc_server_with_manager(port, table_manager, entity_filter, None, never).await
}

pub async fn start_grpc_server_with_manager(
    port: u16,
    table_manager: Arc<TableManager>,
    entity_filter: Option<String>,
    _log_tx: Option<mpsc::Sender<String>>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let addr = format!("{}:{}", host, port).parse()?;
    
    let db = MyDatabase::new(table_manager, entity_filter);
    
    info!("gRPC Server listening on {}", addr);

    tonic::transport::Server::builder()
        .add_service(DatabaseServer::new(db))
        .serve_with_shutdown(addr, async move {
            shutdown.notified().await;
            info!("gRPC server shut down");
        })
        .await?;

    Ok(())
}
