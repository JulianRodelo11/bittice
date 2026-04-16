use anyhow::{anyhow, bail, Result};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use crate::core::date_utils::is_date_format;
use crate::core::expression::{evaluate, extract_fields, parse_expression};
use crate::core::saved_queries::{SavedJoin, SavedOrderBy, SavedQuery, SavedSelectField};
use crate::core::types::{AggregationResult, AuthContext, ComparisonOp, FieldType, Filter, LogicalOp, QueryResult, SortDirection};
use crate::server::table_manager::TableManager;

const FETCH_PAGE_SIZE: usize = 100;
const JOIN_RESULT_LIMIT: usize = 200_000;
const JOIN_SEPARATOR: char = '\u{1f}';

#[derive(Clone, Debug)]
struct QualifiedField {
    alias: String,
    field: String,
    qualified: String,
}

#[derive(Clone, Debug)]
struct Projection {
    source: QualifiedField,
    header: String,
}

#[derive(Clone, Debug)]
struct ResolvedFilter {
    field: QualifiedField,
    op: ComparisonOp,
    value: String,
    field_type: Option<FieldType>,
}

#[derive(Clone, Debug)]
struct ResolvedOrderBy {
    field: QualifiedField,
    direction: SortDirection,
}

#[derive(Clone, Debug)]
struct ResolvedJoinCondition {
    existing_side: QualifiedField,
    joining_side: QualifiedField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug)]
struct ResolvedJoin {
    table: String,
    alias: String,
    kind: JoinKind,
    conditions: Vec<ResolvedJoinCondition>,
}

type FlatRow = HashMap<String, String>;

pub fn execute_join_query(
    query: &SavedQuery,
    params_map: &HashMap<String, String>,
    table_manager: Arc<TableManager>,
    selected_fields_override: Option<Vec<String>>,
    limit: usize,
    offset: usize,
    auth_context: Option<&AuthContext>,
) -> Result<QueryResult> {
    if !query.is_multi_table() {
        bail!("join executor requires a multi-table query");
    }

    let start_time = Instant::now();
    let base_alias = query.base_alias();
    let filters_op = match query.filters_op.as_str() {
        "Or" => LogicalOp::Or,
        _ => LogicalOp::And,
    };
    let projections = resolve_projections(query, selected_fields_override, &base_alias)?;
    let resolved_filters = resolve_filters(query, params_map, &base_alias)?;
    let resolved_order_by = resolve_order_by(&query.order_by, &base_alias)?;
    let resolved_aggregations = resolve_aggregations(&query.aggregations, params_map)?;
    let joins = resolve_joins(query, &base_alias)?;

    if projections.is_empty() && resolved_aggregations.is_empty() {
        bail!("multi-table queries require selected fields, select projections, or aggregations");
    }

    let mut needed_fields = collect_needed_fields(&base_alias, &projections, &resolved_filters, &resolved_order_by, &joins, &resolved_aggregations);
    ensure_required_field(&mut needed_fields, &base_alias, "PK");

    let base_pushdown_filters: Vec<Filter> = resolved_filters
        .iter()
        .filter(|filter| filter.field.alias == base_alias)
        .map(|filter| Filter {
            field: filter.field.field.clone(),
            op: filter.op,
            value: filter.value.clone(),
            field_type: filter.field_type,
            value_options: vec![],
        })
        .collect();

    let base_fields = sorted_fields(needed_fields.get(&base_alias), &base_alias)?;
    let mut current_rows = fetch_table_rows(
        &query.entity,
        &query.table,
        &base_alias,
        &base_fields,
        &base_pushdown_filters,
        table_manager.clone(),
        auth_context,
    )?;

    for join in &joins {
        let join_fields = sorted_fields(needed_fields.get(&join.alias), &join.alias)?;
        let join_rows = fetch_table_rows(
            &query.entity,
            &join.table,
            &join.alias,
            &join_fields,
            &[],
            table_manager.clone(),
            None,
        )?;

        let join_index = build_join_index(&join_rows, &join.conditions);
        current_rows = apply_join(current_rows, join, &join_index)?;
    }

    current_rows = apply_filters(current_rows, &resolved_filters, &filters_op);
    let total_found = current_rows.len();
    let aggregation_results = if resolved_aggregations.is_empty() {
        None
    } else {
        Some(run_aggregations(&current_rows, &resolved_aggregations, &base_alias)?)
    };

    if !resolved_order_by.is_empty() {
        current_rows.sort_by(|left, right| compare_rows(left, right, &resolved_order_by));
    }

    let capped_limit = limit.min(100);
    let paged_rows: Vec<&FlatRow> = if capped_limit == 0 {
        Vec::new()
    } else {
        current_rows.iter().skip(offset).take(capped_limit).collect()
    };

    let headers = projections.iter().map(|projection| projection.header.clone()).collect::<Vec<_>>();
    let rows = paged_rows
        .into_iter()
        .map(|row| {
            projections
                .iter()
                .map(|projection| row.get(&projection.source.qualified).cloned().unwrap_or_default())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let debug = format!(
        "JoinExec: {} join(s), BaseRows: {}, ResultRows: {}",
        joins.len(),
        current_rows.len(),
        total_found
    );

    Ok(QueryResult {
        headers,
        rows,
        row_ids: None,
        total_found,
        execution_time_micros: start_time.elapsed().as_micros(),
        debug_info: Some(debug),
        aggregations: aggregation_results,
    })
}

fn resolve_projections(
    query: &SavedQuery,
    selected_fields_override: Option<Vec<String>>,
    base_alias: &str,
) -> Result<Vec<Projection>> {
    if let Some(fields) = selected_fields_override {
        return fields
            .into_iter()
            .map(|field| {
                let qualified = parse_qualified_field(&field, base_alias)?;
                Ok(Projection {
                    header: field,
                    source: qualified,
                })
            })
            .collect();
    }

    if !query.select.is_empty() {
        return query
            .select
            .iter()
            .map(|select| projection_from_select(select, base_alias))
            .collect();
    }

    query
        .selected_fields
        .iter()
        .map(|field| {
            let qualified = parse_qualified_field(field, base_alias)?;
            Ok(Projection {
                header: field.clone(),
                source: qualified,
            })
        })
        .collect()
}

fn projection_from_select(select: &SavedSelectField, base_alias: &str) -> Result<Projection> {
    let source = parse_qualified_field(&select.field, base_alias)?;
    Ok(Projection {
        header: select.output_name.clone().unwrap_or_else(|| select.field.clone()),
        source,
    })
}

fn resolve_filters(query: &SavedQuery, params_map: &HashMap<String, String>, base_alias: &str) -> Result<Vec<ResolvedFilter>> {
    query
        .filters
        .iter()
        .map(|filter| {
            let field = parse_qualified_field(&filter.field, base_alias)?;
            let value = resolve_param(&filter.value, params_map)?;
            Ok(ResolvedFilter {
                field,
                op: ComparisonOp::from_str(&filter.op),
                value,
                field_type: filter.field_type,
            })
        })
        .collect()
}

fn resolve_order_by(order_by: &[SavedOrderBy], base_alias: &str) -> Result<Vec<ResolvedOrderBy>> {
    order_by
        .iter()
        .map(|order| {
            Ok(ResolvedOrderBy {
                field: parse_qualified_field(&order.field, base_alias)?,
                direction: if order.direction == "Desc" {
                    SortDirection::Desc
                } else {
                    SortDirection::Asc
                },
            })
        })
        .collect()
}

fn resolve_aggregations(aggregations: &[serde_json::Value], params_map: &HashMap<String, String>) -> Result<Vec<serde_json::Value>> {
    let mut resolved = aggregations.to_vec();
    for aggregation in &mut resolved {
        if let Some(object) = aggregation.as_object_mut().and_then(|obj| obj.values_mut().next()).and_then(|value| value.as_object_mut()) {
            for value in object.values_mut() {
                if let Some(raw) = value.as_str() {
                    if raw.starts_with('$') {
                        let param = resolve_param(raw, params_map)?;
                        if let Ok(number) = param.parse::<u64>() {
                            *value = serde_json::json!(number);
                        } else {
                            *value = serde_json::json!(param);
                        }
                    }
                }
            }
        }
    }
    Ok(resolved)
}

fn resolve_joins(query: &SavedQuery, base_alias: &str) -> Result<Vec<ResolvedJoin>> {
    let mut bound_aliases = HashSet::new();
    bound_aliases.insert(base_alias.to_string());

    query
        .joins
        .iter()
        .map(|join| resolve_join(join, base_alias, &mut bound_aliases))
        .collect()
}

fn resolve_join(join: &SavedJoin, base_alias: &str, bound_aliases: &mut HashSet<String>) -> Result<ResolvedJoin> {
    let alias = join
        .alias
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(join.table.as_str())
        .to_string();

    if bound_aliases.contains(&alias) {
        bail!("duplicate join alias '{}'", alias);
    }
    if join.on.is_empty() {
        bail!("join '{}' requires at least one ON condition", alias);
    }

    let kind = match join.join_type.to_lowercase().as_str() {
        "inner" => JoinKind::Inner,
        "left" => JoinKind::Left,
        other => bail!("unsupported join type '{}'", other),
    };

    let mut conditions = Vec::with_capacity(join.on.len());
    for condition in &join.on {
        if !condition.op.eq_ignore_ascii_case("Eq") {
            bail!("only Eq join conditions are supported");
        }

        let left = parse_qualified_field(&condition.left, base_alias)?;
        let right = parse_qualified_field(&condition.right, base_alias)?;

        let resolved = if left.alias == alias && bound_aliases.contains(&right.alias) {
            ResolvedJoinCondition {
                existing_side: right,
                joining_side: left,
            }
        } else if right.alias == alias && bound_aliases.contains(&left.alias) {
            ResolvedJoinCondition {
                existing_side: left,
                joining_side: right,
            }
        } else {
            bail!(
                "join '{}' must connect the new alias to an already-bound alias in every ON condition",
                alias
            );
        };

        conditions.push(resolved);
    }

    bound_aliases.insert(alias.clone());
    Ok(ResolvedJoin {
        table: join.table.clone(),
        alias,
        kind,
        conditions,
    })
}

fn collect_needed_fields(
    base_alias: &str,
    projections: &[Projection],
    filters: &[ResolvedFilter],
    order_by: &[ResolvedOrderBy],
    joins: &[ResolvedJoin],
    aggregations: &[serde_json::Value],
) -> HashMap<String, HashSet<String>> {
    let mut needed = HashMap::<String, HashSet<String>>::new();
    needed.entry(base_alias.to_string()).or_default();

    for projection in projections {
        ensure_required_field(&mut needed, &projection.source.alias, &projection.source.field);
    }
    for filter in filters {
        ensure_required_field(&mut needed, &filter.field.alias, &filter.field.field);
    }
    for order in order_by {
        ensure_required_field(&mut needed, &order.field.alias, &order.field.field);
    }
    for join in joins {
        ensure_required_field(&mut needed, &join.alias, "PK");
        for condition in &join.conditions {
            ensure_required_field(&mut needed, &condition.existing_side.alias, &condition.existing_side.field);
            ensure_required_field(&mut needed, &condition.joining_side.alias, &condition.joining_side.field);
        }
    }
    collect_aggregation_fields(&mut needed, aggregations, base_alias);

    needed
}

fn collect_aggregation_fields(needed: &mut HashMap<String, HashSet<String>>, aggregations: &[serde_json::Value], base_alias: &str) {
    for aggregation in aggregations {
        if let Some(object) = aggregation.as_object() {
            for (agg_type, params) in object {
                match agg_type.as_str() {
                    "GroupBy" | "TopN" => {
                        if let Some(field) = params.get("field").and_then(|value| value.as_str()) {
                            if let Ok(qualified) = parse_qualified_field(field, base_alias) {
                                ensure_required_field(needed, &qualified.alias, &qualified.field);
                            }
                        }
                    }
                    "Sum" => {
                        if let Some(group_by) = params.get("group_by").and_then(|value| value.as_str()) {
                            if let Ok(qualified) = parse_qualified_field(group_by, base_alias) {
                                ensure_required_field(needed, &qualified.alias, &qualified.field);
                            }
                        }
                        if let Some(field) = params.get("field").and_then(|value| value.as_str()) {
                            if let Ok(qualified) = parse_qualified_field(field, base_alias) {
                                ensure_required_field(needed, &qualified.alias, &qualified.field);
                            }
                        }
                        if let Some(expression) = params.get("expression").and_then(|value| value.as_str()) {
                            if let Ok(parsed) = parse_expression(expression) {
                                for field in extract_fields(&parsed) {
                                    if let Ok(qualified) = parse_qualified_field(&field, base_alias) {
                                        ensure_required_field(needed, &qualified.alias, &qualified.field);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn ensure_required_field(needed: &mut HashMap<String, HashSet<String>>, alias: &str, field: &str) {
    needed.entry(alias.to_string()).or_default().insert(field.to_string());
}

fn sorted_fields(fields: Option<&HashSet<String>>, alias: &str) -> Result<Vec<String>> {
    let mut sorted = fields
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect::<Vec<_>>();
    if sorted.is_empty() {
        bail!("no fields resolved for alias '{}'", alias);
    }
    sorted.sort();
    Ok(sorted)
}

fn fetch_table_rows(
    entity: &str,
    table_name: &str,
    alias: &str,
    fields: &[String],
    filters: &[Filter],
    table_manager: Arc<TableManager>,
    auth_context: Option<&AuthContext>,
) -> Result<Vec<FlatRow>> {
    let table_lock = table_manager.get_table(entity, table_name)?;
    let mut table = table_lock.write().unwrap();
    let _ = table.reload_if_needed();

    let mut rows = Vec::new();
    let mut offset = 0;

    loop {
        let result = table.search(fields, filters, &LogicalOp::And, &[], &[], FETCH_PAGE_SIZE, offset, auth_context)?;
        let batch_rows = if result.rows.is_empty() {
            if let Some(row_ids) = &result.row_ids {
                table.get_rows_batch(fields, row_ids)?
            } else {
                Vec::new()
            }
        } else {
            result.rows
        };

        if batch_rows.is_empty() {
            break;
        }

        for batch_row in &batch_rows {
            let mut mapped = FlatRow::with_capacity(fields.len());
            for (index, field) in fields.iter().enumerate() {
                mapped.insert(qualify(alias, field), batch_row.get(index).cloned().unwrap_or_default());
            }
            rows.push(mapped);
        }

        offset += batch_rows.len();
        if offset >= result.total_found {
            break;
        }
    }

    Ok(rows)
}

fn build_join_index<'a>(rows: &'a [FlatRow], conditions: &[ResolvedJoinCondition]) -> HashMap<String, Vec<&'a FlatRow>> {
    let mut index: HashMap<String, Vec<&FlatRow>> = HashMap::new();
    for row in rows {
        if let Some(key) = compose_key(row, conditions.iter().map(|condition| &condition.joining_side)) {
            index.entry(key).or_default().push(row);
        }
    }
    index
}

fn apply_join(current_rows: Vec<FlatRow>, join: &ResolvedJoin, join_index: &HashMap<String, Vec<&FlatRow>>) -> Result<Vec<FlatRow>> {
    let mut joined_rows = Vec::new();
    for row in current_rows {
        let key = compose_key(&row, join.conditions.iter().map(|condition| &condition.existing_side));
        match key.and_then(|lookup| join_index.get(&lookup)) {
            Some(matches) => {
                for matched_row in matches {
                    let mut merged = row.clone();
                    for (field, value) in matched_row.iter() {
                        merged.insert(field.clone(), value.clone());
                    }
                    joined_rows.push(merged);
                }
            }
            None if join.kind == JoinKind::Left => joined_rows.push(row),
            None => {}
        }

        if joined_rows.len() > JOIN_RESULT_LIMIT {
            bail!("multi-table query exceeded the in-memory join result limit ({})", JOIN_RESULT_LIMIT);
        }
    }

    Ok(joined_rows)
}

fn apply_filters(rows: Vec<FlatRow>, filters: &[ResolvedFilter], filters_op: &LogicalOp) -> Vec<FlatRow> {
    if filters.is_empty() {
        return rows;
    }

    rows.into_iter()
        .filter(|row| match filters_op {
            LogicalOp::And => filters.iter().all(|filter| evaluate_filter(row, filter)),
            LogicalOp::Or => filters.iter().any(|filter| evaluate_filter(row, filter)),
        })
        .collect()
}

fn evaluate_filter(row: &FlatRow, filter: &ResolvedFilter) -> bool {
    let Some(actual) = row.get(&filter.field.qualified) else {
        return false;
    };

    match filter.op {
        ComparisonOp::Eq => actual == &filter.value,
        ComparisonOp::Ne => actual != &filter.value,
        ComparisonOp::Gt => compare_values(actual, &filter.value, filter.field_type) == Some(Ordering::Greater),
        ComparisonOp::Gte => matches!(compare_values(actual, &filter.value, filter.field_type), Some(Ordering::Greater) | Some(Ordering::Equal)),
        ComparisonOp::Lt => compare_values(actual, &filter.value, filter.field_type) == Some(Ordering::Less),
        ComparisonOp::Lte => matches!(compare_values(actual, &filter.value, filter.field_type), Some(Ordering::Less) | Some(Ordering::Equal)),
        ComparisonOp::In => filter.value.split(',').map(|value| value.trim()).any(|value| value == actual),
    }
}

fn compare_rows(left: &FlatRow, right: &FlatRow, order_by: &[ResolvedOrderBy]) -> Ordering {
    for order in order_by {
        let left_value = left.get(&order.field.qualified).map(String::as_str).unwrap_or("");
        let right_value = right.get(&order.field.qualified).map(String::as_str).unwrap_or("");
        let ordering = compare_values(left_value, right_value, None).unwrap_or_else(|| left_value.cmp(right_value));
        if ordering != Ordering::Equal {
            return if order.direction == SortDirection::Desc {
                ordering.reverse()
            } else {
                ordering
            };
        }
    }
    Ordering::Equal
}

fn compare_values(left: &str, right: &str, field_type: Option<FieldType>) -> Option<Ordering> {
    match field_type {
        Some(FieldType::Int) | Some(FieldType::Float) => compare_numeric(left, right),
        Some(FieldType::Date) => compare_dates(left, right),
        _ => compare_numeric(left, right)
            .or_else(|| compare_dates(left, right))
            .or_else(|| Some(left.cmp(right))),
    }
}

fn compare_numeric(left: &str, right: &str) -> Option<Ordering> {
    let left_num = left.parse::<f64>().ok()?;
    let right_num = right.parse::<f64>().ok()?;
    left_num.partial_cmp(&right_num)
}

fn compare_dates(left: &str, right: &str) -> Option<Ordering> {
    if is_date_format(left) && is_date_format(right) {
        Some(left.cmp(right))
    } else {
        None
    }
}

fn run_aggregations(rows: &[FlatRow], aggregations: &[serde_json::Value], base_alias: &str) -> Result<Vec<AggregationResult>> {
    let mut results = Vec::new();
    for aggregation in aggregations {
        let Some(object) = aggregation.as_object() else {
            continue;
        };
        let Some((agg_type, params)) = object.iter().next() else {
            continue;
        };

        match agg_type.as_str() {
            "Count" => {
                results.push(AggregationResult {
                    headers: vec![],
                    rows: vec![],
                    summary: Some(rows.len() as f64),
                });
            }
            "GroupBy" | "TopN" => {
                let field = params
                    .get("field")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| anyhow!("{} aggregation requires 'field'", agg_type))?;
                let qualified = parse_qualified_field(field, base_alias)?;
                let mut counts = HashMap::<String, u64>::new();
                for row in rows {
                    let value = row.get(&qualified.qualified).cloned().unwrap_or_default();
                    *counts.entry(value).or_insert(0) += 1;
                }

                let mut entries = counts.into_iter().collect::<Vec<_>>();
                entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
                if agg_type == "TopN" {
                    let n = params.get("n").and_then(|value| value.as_u64()).unwrap_or(10) as usize;
                    entries.truncate(n);
                }

                let total = entries.iter().map(|(_, count)| *count).sum::<u64>() as f64;
                results.push(AggregationResult {
                    headers: vec![field.to_string(), "count".to_string()],
                    rows: entries
                        .into_iter()
                        .map(|(value, count)| vec![value, count.to_string()])
                        .collect(),
                    summary: Some(total),
                });
            }
            "Sum" => {
                let group_by = params.get("group_by").and_then(|value| value.as_str());
                let expression = params
                    .get("expression")
                    .and_then(|value| value.as_str())
                    .or_else(|| params.get("field").and_then(|value| value.as_str()))
                    .unwrap_or("0");
                let parsed = parse_expression(expression)?;
                let mut grouped = HashMap::<String, f64>::new();
                let mut total = 0.0;

                for row in rows {
                    let context = row
                        .iter()
                        .filter_map(|(field, value)| value.parse::<f64>().ok().map(|number| (field.clone(), number)))
                        .collect::<HashMap<_, _>>();
                    let value = evaluate(&parsed, &context);
                    total += value;

                    if let Some(group_field) = group_by {
                        let group = parse_qualified_field(group_field, base_alias)?;
                        let group_value = row.get(&group.qualified).cloned().unwrap_or_default();
                        *grouped.entry(group_value).or_insert(0.0) += value;
                    }
                }

                if let Some(group_field) = group_by {
                    let mut entries = grouped.into_iter().collect::<Vec<_>>();
                    entries.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));
                    results.push(AggregationResult {
                        headers: vec![group_field.to_string(), "sum".to_string()],
                        rows: entries
                            .into_iter()
                            .map(|(group, sum)| vec![group, format!("{:.2}", sum)])
                            .collect(),
                        summary: Some(total),
                    });
                } else {
                    results.push(AggregationResult {
                        headers: vec![],
                        rows: vec![],
                        summary: Some(total),
                    });
                }
            }
            other => bail!("unsupported aggregation '{}' for multi-table query", other),
        }
    }

    Ok(results)
}

fn resolve_param(raw: &str, params_map: &HashMap<String, String>) -> Result<String> {
    if let Some(key) = raw.strip_prefix('$') {
        params_map
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("missing param '{}'", key))
    } else {
        Ok(raw.to_string())
    }
}

fn parse_qualified_field(value: &str, base_alias: &str) -> Result<QualifiedField> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("field reference cannot be empty");
    }
    if trimmed == "*" {
        bail!("wildcard fields are not supported in multi-table queries");
    }

    let mut parts = trimmed.splitn(2, '.');
    let first = parts.next().unwrap_or_default().trim();
    let second = parts.next().map(str::trim);
    let (alias, field) = match second {
        Some(field) if !field.is_empty() => (first.to_string(), field.to_string()),
        _ => (base_alias.to_string(), first.to_string()),
    };

    Ok(QualifiedField {
        qualified: qualify(&alias, &field),
        alias,
        field,
    })
}

fn compose_key<'a>(row: &FlatRow, fields: impl Iterator<Item = &'a QualifiedField>) -> Option<String> {
    let mut values = Vec::new();
    for field in fields {
        values.push(row.get(&field.qualified)?.clone());
    }
    Some(values.join(&JOIN_SEPARATOR.to_string()))
}

fn qualify(alias: &str, field: &str) -> String {
    format!("{}.{}", alias, field)
}