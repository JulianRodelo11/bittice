use anyhow::{anyhow, bail, Result};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use crate::core::expression::{evaluate, extract_fields, parse_expression};
use crate::core::saved_queries::{SavedFilterTreeNode, SavedJoin, SavedOrderBy, SavedQuery, SavedSelectField};
use crate::core::types::{compare_filter_value, compare_values, AggregationResult, AuthContext, ComparisonOp, FieldType, Filter, LogicalOp, QueryResult, SortDirection};
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
    header: String,
    kind: ProjectionKind,
}

#[derive(Clone, Debug)]
enum ProjectionKind {
    Source(QualifiedField),
    Computed {
        qualified: String,
        expr: crate::core::expression::Expr,
        fields: Vec<QualifiedField>,
    },
}

#[derive(Clone, Debug)]
struct ResolvedFilter {
    field: QualifiedField,
    op: ComparisonOp,
    value: String,
    value_to: Option<String>,
    value_options: Vec<String>,
    field_type: Option<FieldType>,
}

#[derive(Clone, Debug)]
enum ResolvedFilterTreeNode {
    Condition(ResolvedFilter),
    Group {
        op: LogicalOp,
        filters: Vec<ResolvedFilterTreeNode>,
    },
}

#[derive(Clone, Debug)]
struct ResolvedHaving {
    op: ComparisonOp,
    value: String,
    value_to: Option<String>,
    value_options: Vec<String>,
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
    entity: String,
    table: String,
    alias: String,
    kind: JoinKind,
    conditions: Vec<ResolvedJoinCondition>,
    /// Synthetic field `{alias}.{name}` filled with match count instead of row-expanding join.
    count_matches_as: Option<String>,
    sum_matches_field: Option<String>,
    sum_matches_as: Option<String>,
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
    let resolved_filter_tree = resolve_filter_tree(query.filter_tree.as_ref(), params_map, &base_alias)?;
    let computed_headers = projections
        .iter()
        .filter_map(|projection| match &projection.kind {
            ProjectionKind::Computed { .. } => Some(projection.header.clone()),
            ProjectionKind::Source(_) => None,
        })
        .collect::<Vec<_>>();
    let resolved_order_by = resolve_order_by(&query.order_by, &base_alias, &computed_headers)?;
    let resolved_aggregations = resolve_aggregations(&query.aggregations, params_map)?;
    let joins = resolve_joins(query, &base_alias)?;
    let left_join_aliases: HashSet<String> = joins
        .iter()
        .filter(|j| j.kind == JoinKind::Left)
        .map(|j| j.alias.clone())
        .collect();
    let post_join_filters: Vec<ResolvedFilter> = resolved_filters
        .iter()
        .filter(|f| !left_join_aliases.contains(&f.field.alias))
        .cloned()
        .collect();
    let mut available_aliases = HashSet::new();
    available_aliases.insert(base_alias.clone());
    let mut filters_applied_early = false;
    let mut total_found = 0usize;

    if projections.is_empty() && resolved_aggregations.is_empty() {
        bail!("multi-table queries require selected fields, select projections, or aggregations");
    }

    let mut needed_fields = collect_needed_fields(&base_alias, &projections, &resolved_filters, resolved_filter_tree.as_ref(), &resolved_order_by, &joins, &resolved_aggregations);
    ensure_required_field(&mut needed_fields, &base_alias, "PK");

    let base_pushdown_filters = collect_pushdown_filters(&resolved_filters, resolved_filter_tree.as_ref(), &base_alias);

    let base_fields = sorted_fields(needed_fields.get(&base_alias), &base_alias)?;
    let t_base_start = Instant::now();
    let mut current_rows = fetch_table_rows(
        &query.entity,
        &query.table,
        &base_alias,
        &base_fields,
        &base_pushdown_filters,
        table_manager.clone(),
        auth_context,
    )?;
    let t_base_ms = t_base_start.elapsed().as_secs_f64() * 1000.0;
    let base_row_count = current_rows.len();
    let mut join_timings: Vec<(String, f64, f64, f64, usize)> = Vec::new();

    for (idx, join) in joins.iter().enumerate() {
        let join_fields = sorted_join_fetch_fields(needed_fields.get(&join.alias), join)?;
        let join_pushdown_filters = collect_pushdown_filters(&resolved_filters, resolved_filter_tree.as_ref(), &join.alias);
        let t_fetch = Instant::now();
        let join_rows = fetch_join_rows(
            join,
            &join_fields,
            &current_rows,
            &join_pushdown_filters,
            table_manager.clone(),
        )?;
        let fetch_ms = t_fetch.elapsed().as_secs_f64() * 1000.0;
        let fetched_count = join_rows.len();
        let t_idx = Instant::now();
        let join_index = build_join_index(&join_rows, &join.conditions);
        let index_ms = t_idx.elapsed().as_secs_f64() * 1000.0;
        let t_apply = Instant::now();
        current_rows = apply_join(current_rows, join, &join_index)?;
        let apply_ms = t_apply.elapsed().as_secs_f64() * 1000.0;
        join_timings.push((join.alias.clone(), fetch_ms, index_ms, apply_ms, fetched_count));
        available_aliases.insert(join.alias.clone());

        // If all filter/order aliases are already available and only LEFT joins remain,
        // trim early to avoid expensive enrichment joins on rows that won't be returned.
        if !filters_applied_early
            && can_apply_early_window(
                &post_join_filters,
                resolved_filter_tree.as_ref(),
                &resolved_order_by,
                &resolved_aggregations,
                &available_aliases,
                &joins[idx + 1..],
            )
        {
            current_rows = apply_filters(current_rows, &post_join_filters, &filters_op);
            filters_applied_early = true;
            if !resolved_order_by.is_empty() {
                current_rows.sort_by(|left, right| compare_rows(left, right, &resolved_order_by));
                total_found = current_rows.len();
                // Honor the caller's limit. The HTTP layer caps it at
                // HARD_MAX_LIMIT (10_000) in server/mod.rs, so the worst case
                // here is bounded. The previous .min(100) silently capped
                // every joined saved op at 100 rows regardless of ?limit=N
                // — diagnostic ops like list-recent-checks need thousands
                // and were paginating only because of this clamp.
                let early_window = offset.saturating_add(limit);
                if early_window > 0 && current_rows.len() > early_window {
                    current_rows.truncate(early_window);
                }
            }
        }
    }

    if !filters_applied_early {
        current_rows = apply_filters(current_rows, &post_join_filters, &filters_op);
    }
    if let Some(filter_tree) = &resolved_filter_tree {
        current_rows = apply_filter_tree(current_rows, filter_tree);
    }
    if projections.iter().any(|projection| matches!(projection.kind, ProjectionKind::Computed { .. })) {
        enrich_rows_with_computed(&mut current_rows, &projections);
    }
    if total_found == 0 {
        total_found = current_rows.len();
    }
    let aggregation_results = if resolved_aggregations.is_empty() {
        None
    } else {
        Some(run_aggregations(&current_rows, &resolved_aggregations, &base_alias)?)
    };

    if !resolved_order_by.is_empty() {
        current_rows.sort_by(|left, right| compare_rows(left, right, &resolved_order_by));
    }

    // Same reasoning as the early-window block above: trust the caller's
    // limit (HARD_MAX_LIMIT-bounded upstream). The previous .min(100) here
    // truncated the actual response payload regardless of ?limit=N, so the
    // HTTP layer's pagination metadata showed per_page=5000 while data
    // arrived at 100 rows — the silent cap that forced the warmer Lambda to
    // paginate aggressively.
    let paged_rows: Vec<&FlatRow> = if limit == 0 {
        Vec::new()
    } else {
        current_rows.iter().skip(offset).take(limit).collect()
    };

    let headers = projections.iter().map(|projection| projection.header.clone()).collect::<Vec<_>>();
    let rows = paged_rows
        .into_iter()
        .map(|row| {
            projections
                .iter()
                .map(|projection| match &projection.kind {
                    ProjectionKind::Source(source) => row.get(&source.qualified).cloned().unwrap_or_default(),
                    ProjectionKind::Computed { qualified, .. } => row.get(qualified).cloned().unwrap_or_default(),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut debug = format!(
        "JoinExec: {} join(s), BaseRows: {}, ResultRows: {}, Base: {:.1}ms (rows={})",
        joins.len(),
        current_rows.len(),
        total_found,
        t_base_ms,
        base_row_count,
    );
    for (alias, fetch_ms, index_ms, apply_ms, fetched) in &join_timings {
        debug.push_str(&format!(
            " | join '{}' fetch={:.1}ms (rows={}) idx={:.1}ms apply={:.1}ms",
            alias, fetch_ms, fetched, index_ms, apply_ms
        ));
    }

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
                    kind: ProjectionKind::Source(qualified),
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
                kind: ProjectionKind::Source(qualified),
            })
        })
        .collect()
}

fn projection_from_select(select: &SavedSelectField, base_alias: &str) -> Result<Projection> {
    let header = select.output_name.clone().unwrap_or_else(|| {
        select.expression.clone().unwrap_or_else(|| select.field.clone())
    });

    if let Some(expression) = &select.expression {
        let expr = parse_expression(expression)?;
        let fields = extract_fields(&expr)
            .into_iter()
            .map(|field| parse_qualified_field(&field, base_alias))
            .collect::<Result<Vec<_>>>()?;

        return Ok(Projection {
            header: header.clone(),
            kind: ProjectionKind::Computed {
                qualified: computed_qualified(&header),
                expr,
                fields,
            },
        });
    }

    let source = parse_qualified_field(&select.field, base_alias)?;
    Ok(Projection {
        header,
        kind: ProjectionKind::Source(source),
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
                value_to: resolve_optional_param(filter.value_to.as_deref(), params_map)?,
                value_options: resolve_param_list(&filter.values, params_map),
                field_type: filter.field_type,
            })
        })
        .collect()
}

fn resolve_filter_tree(
    filter_tree: Option<&SavedFilterTreeNode>,
    params_map: &HashMap<String, String>,
    base_alias: &str,
) -> Result<Option<ResolvedFilterTreeNode>> {
    filter_tree
        .map(|node| resolve_filter_tree_node(node, params_map, base_alias))
        .transpose()
}

fn resolve_filter_tree_node(
    node: &SavedFilterTreeNode,
    params_map: &HashMap<String, String>,
    base_alias: &str,
) -> Result<ResolvedFilterTreeNode> {
    match node {
        SavedFilterTreeNode::Condition(filter) => {
            let field = parse_qualified_field(&filter.field, base_alias)?;
            Ok(ResolvedFilterTreeNode::Condition(ResolvedFilter {
                field,
                op: ComparisonOp::from_str(&filter.op),
                value: resolve_param(&filter.value, params_map)?,
                value_to: resolve_optional_param(filter.value_to.as_deref(), params_map)?,
                value_options: resolve_param_list(&filter.values, params_map),
                field_type: filter.field_type,
            }))
        }
        SavedFilterTreeNode::Group(group) => Ok(ResolvedFilterTreeNode::Group {
            op: parse_logical_op(&group.op),
            filters: group
                .filters
                .iter()
                .map(|child| resolve_filter_tree_node(child, params_map, base_alias))
                .collect::<Result<Vec<_>>>()?,
        }),
    }
}

fn resolve_order_by(order_by: &[SavedOrderBy], base_alias: &str, computed_headers: &[String]) -> Result<Vec<ResolvedOrderBy>> {
    order_by
        .iter()
        .map(|order| {
            Ok(ResolvedOrderBy {
                field: if computed_headers.iter().any(|header| header == &order.field) {
                    QualifiedField {
                        alias: "__computed".to_string(),
                        field: order.field.clone(),
                        qualified: computed_qualified(&order.field),
                    }
                } else {
                    parse_qualified_field(&order.field, base_alias)?
                },
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
        .map(|join| resolve_join(join, base_alias, &mut bound_aliases, &query.entity))
        .collect()
}

fn resolve_join(join: &SavedJoin, base_alias: &str, bound_aliases: &mut HashSet<String>, base_entity: &str) -> Result<ResolvedJoin> {
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

    let count_matches_as = join
        .count_matches_as
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(name) = &count_matches_as {
        if name.contains('.') {
            bail!("join '{}' count_matches_as must be an unqualified field name, got '{}'", alias, name);
        }
    }

    let sum_matches_field = join
        .sum_matches_field
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let sum_matches_as = join
        .sum_matches_as
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let has_sum_field = sum_matches_field.is_some();
    let has_sum_as = sum_matches_as.is_some();
    if has_sum_field != has_sum_as {
        bail!(
            "join '{}' sum_matches_field and sum_matches_as must both be set or both omitted",
            alias
        );
    }
    if let Some(name) = &sum_matches_field {
        if name.contains('.') {
            bail!("join '{}' sum_matches_field must be an unqualified column name, got '{}'", alias, name);
        }
    }
    if let Some(name) = &sum_matches_as {
        if name.contains('.') {
            bail!("join '{}' sum_matches_as must be an unqualified field name, got '{}'", alias, name);
        }
    }
    if let (Some(f), Some(a)) = (&sum_matches_field, &sum_matches_as) {
        if f.eq_ignore_ascii_case(a) {
            bail!(
                "join '{}' sum_matches_field and sum_matches_as must differ (got '{}' for both)",
                alias,
                f
            );
        }
    }

    bound_aliases.insert(alias.clone());
    Ok(ResolvedJoin {
        entity: join.entity.clone().unwrap_or_else(|| base_entity.to_string()),
        table: join.table.clone(),
        alias,
        kind,
        conditions,
        count_matches_as,
        sum_matches_field,
        sum_matches_as,
    })
}

fn collect_needed_fields(
    base_alias: &str,
    projections: &[Projection],
    filters: &[ResolvedFilter],
    filter_tree: Option<&ResolvedFilterTreeNode>,
    order_by: &[ResolvedOrderBy],
    joins: &[ResolvedJoin],
    aggregations: &[serde_json::Value],
) -> HashMap<String, HashSet<String>> {
    let mut needed = HashMap::<String, HashSet<String>>::new();
    needed.entry(base_alias.to_string()).or_default();

    for projection in projections {
        match &projection.kind {
            ProjectionKind::Source(source) => {
                ensure_required_field(&mut needed, &source.alias, &source.field);
            }
            ProjectionKind::Computed { fields, .. } => {
                for field in fields {
                    ensure_required_field(&mut needed, &field.alias, &field.field);
                }
            }
        }
    }
    for filter in filters {
        ensure_required_field(&mut needed, &filter.field.alias, &filter.field.field);
    }
    if let Some(filter_tree) = filter_tree {
        collect_filter_tree_fields(&mut needed, filter_tree);
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
        if let Some(ref sf) = join.sum_matches_field {
            ensure_required_field(&mut needed, &join.alias, sf);
        }
    }
    collect_aggregation_fields(&mut needed, aggregations, base_alias);

    needed
}

fn collect_filter_tree_fields(needed: &mut HashMap<String, HashSet<String>>, node: &ResolvedFilterTreeNode) {
    match node {
        ResolvedFilterTreeNode::Condition(filter) => {
            ensure_required_field(needed, &filter.field.alias, &filter.field.field);
        }
        ResolvedFilterTreeNode::Group { filters, .. } => {
            for child in filters {
                collect_filter_tree_fields(needed, child);
            }
        }
    }
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
                    "Avg" | "Min" | "Max" => {
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
                    "CountDistinct" => {
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

/// Fields to load from storage for a join; excludes synthetic outputs (`count_matches_as`, `sum_matches_as`).
fn sorted_join_fetch_fields(fields: Option<&HashSet<String>>, join: &ResolvedJoin) -> Result<Vec<String>> {
    let Some(set) = fields else {
        bail!("no fields resolved for alias '{}'", join.alias);
    };
    let filtered: HashSet<String> = set
        .iter()
        .filter(|f| {
            join.count_matches_as.as_deref() != Some(f.as_str())
                && join.sum_matches_as.as_deref() != Some(f.as_str())
        })
        .cloned()
        .collect();
    sorted_fields(Some(&filtered), &join.alias)
}

fn collect_pushdown_filters(
    resolved_filters: &[ResolvedFilter],
    resolved_filter_tree: Option<&ResolvedFilterTreeNode>,
    alias: &str,
) -> Vec<Filter> {
    resolved_filters
        .iter()
        .filter(|_| resolved_filter_tree.is_none())
        .filter(|filter| filter.field.alias == alias)
        .map(|filter| Filter {
            field: filter.field.field.clone(),
            op: filter.op,
            value: filter.value.clone(),
            value_to: filter.value_to.clone(),
            field_type: filter.field_type,
            value_options: filter.value_options.clone(),
        })
        .collect()
}

fn fetch_join_rows(
    join: &ResolvedJoin,
    fields: &[String],
    current_rows: &[FlatRow],
    pushdown_filters: &[Filter],
    table_manager: Arc<TableManager>,
) -> Result<Vec<FlatRow>> {
    let lookup_filters = build_join_lookup_filters(current_rows, join, pushdown_filters);

    if let Some(filter_sets) = lookup_filters {
        let t_get = Instant::now();
        let table_lock = table_manager.get_table_for_query(&join.entity, &join.table)?;
        let get_ms = t_get.elapsed().as_secs_f64() * 1000.0;
        let t_lock = Instant::now();
        // Read lock: search/get_rows_batch only need &Table; using a write lock here forced
        // every join lookup to wait behind CDC writes on hot tables, turning
        // sub-100ms queries into multi-second ones under streaming load.
        let table = table_lock.read().unwrap();
        let lock_ms = t_lock.elapsed().as_secs_f64() * 1000.0;
        if get_ms > 5.0 || lock_ms > 5.0 {
            tracing::warn!(
                "join: get_table {}/{} get={:.1}ms lock={:.1}ms",
                join.entity, join.table, get_ms, lock_ms
            );
        }

        let mut rows = Vec::new();
        let mut seen_fingerprints = HashSet::new();
        for filters in filter_sets {
            let batch = fetch_rows_from_table(&table, &join.alias, fields, &filters, None)?;
            for row in batch {
                // Many base rows can share the same join key (e.g. pa.usuarioProductoId).
                // Lookup batches re-fetch the identical joined row once per key; appending
                // blindly makes build_join_index treat one target row as N matches and fans
                // out a 1:1 inner join into a cartesian product.
                if seen_fingerprints.insert(flat_row_fingerprint(&row)) {
                    rows.push(row);
                }
            }
        }
        Ok(rows)
    } else {
        fetch_table_rows(
            &join.entity,
            &join.table,
            &join.alias,
            fields,
            pushdown_filters,
            table_manager,
            None,
        )
    }
}

fn build_join_lookup_filters(
    current_rows: &[FlatRow],
    join: &ResolvedJoin,
    pushdown_filters: &[Filter],
) -> Option<Vec<Vec<Filter>>> {
    if join.conditions.is_empty() {
        return None;
    }

    let mut seen = HashSet::new();
    let mut filter_sets = Vec::new();

    for row in current_rows {
        let mut filters = Vec::with_capacity(join.conditions.len() + pushdown_filters.len());
        let mut key_parts = Vec::with_capacity(join.conditions.len());

        for condition in &join.conditions {
            let value_raw = row.get(&condition.existing_side.qualified)?.clone();
            let value = normalize_join_key_fragment(&value_raw);
            key_parts.push(format!("{}={}", condition.joining_side.field, value));
            filters.push(Filter {
                field: condition.joining_side.field.clone(),
                op: ComparisonOp::Eq,
                value,
                value_to: None,
                field_type: None,
                value_options: vec![],
            });
        }

        filters.extend_from_slice(pushdown_filters);

        let dedupe_key = key_parts.join("\u{1f}");
        if seen.insert(dedupe_key) {
            filter_sets.push(filters);
        }
    }

    Some(filter_sets)
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
    let table_lock = table_manager.get_table_for_query(entity, table_name)?;
    let table = table_lock.read().unwrap();
    fetch_rows_from_table(&table, alias, fields, filters, auth_context)
}

fn fetch_rows_from_table(
    table: &crate::core::storage::table::Table,
    alias: &str,
    fields: &[String],
    filters: &[Filter],
    auth_context: Option<&AuthContext>,
) -> Result<Vec<FlatRow>> {
    if let Some(probe_rows) = table.probe_fetch_rows(fields, filters)? {
        return Ok(probe_rows
            .into_iter()
            .map(|batch_row| flat_row_from_batch(alias, fields, &batch_row))
            .collect());
    }

    let mut rows = Vec::new();
    let mut offset = 0;
    let mut iter = 0;
    let mut total_search_ms = 0.0;
    let mut total_fetch_ms = 0.0;

    loop {
        iter += 1;
        let t_s = Instant::now();
        let result = table.search(fields, filters, &LogicalOp::And, &[], &[], FETCH_PAGE_SIZE, offset, auth_context)?;
        let s_ms = t_s.elapsed().as_secs_f64() * 1000.0;
        total_search_ms += s_ms;
        let t_f = Instant::now();
        let batch_rows = if result.rows.is_empty() {
            if let Some(row_ids) = &result.row_ids {
                table.get_rows_batch(fields, row_ids)?
            } else {
                Vec::new()
            }
        } else {
            result.rows
        };
        let f_ms = t_f.elapsed().as_secs_f64() * 1000.0;
        total_fetch_ms += f_ms;

        if batch_rows.is_empty() {
            break;
        }

        for batch_row in &batch_rows {
            rows.push(flat_row_from_batch(alias, fields, batch_row));
        }

        offset += batch_rows.len();
        if offset >= result.total_found {
            break;
        }
    }

    if total_search_ms + total_fetch_ms > 50.0 {
        tracing::warn!(
            "fetch_rows_from_table '{}' iters={} search={:.1}ms fetch={:.1}ms total_rows={} filter0_field={:?} filter0_value={:?}",
            alias, iter, total_search_ms, total_fetch_ms, rows.len(),
            filters.first().map(|f| f.field.clone()),
            filters.first().map(|f| f.value.clone())
        );
    }

    Ok(rows)
}

fn flat_row_from_batch(alias: &str, fields: &[String], batch_row: &[String]) -> FlatRow {
    let mut mapped = FlatRow::with_capacity(fields.len());
    for (index, field) in fields.iter().enumerate() {
        mapped.insert(
            qualify(alias, field),
            batch_row.get(index).cloned().unwrap_or_default(),
        );
    }
    mapped
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
    let aggregate_join = join.count_matches_as.is_some()
        || (join.sum_matches_field.is_some() && join.sum_matches_as.is_some());
    if aggregate_join {
        let qualified_count = join
            .count_matches_as
            .as_ref()
            .map(|count_field| qualify(&join.alias, count_field));
        let sum_in_q = join
            .sum_matches_field
            .as_ref()
            .map(|sum_field| qualify(&join.alias, sum_field));
        let qualified_sum_out = join
            .sum_matches_as
            .as_ref()
            .map(|sum_as| qualify(&join.alias, sum_as));

        let mut joined_rows = Vec::new();
        for row in current_rows {
            let key = compose_key(&row, join.conditions.iter().map(|condition| &condition.existing_side));
            let lookup = key.as_ref().and_then(|lookup_key| join_index.get(lookup_key));

            match lookup {
                Some(matches) => {
                    if matches.is_empty() {
                        if join.kind == JoinKind::Left {
                            let mut merged = row.clone();
                            if let Some(qc) = &qualified_count {
                                merged.insert(qc.clone(), "0".to_string());
                            }
                            if let Some(qout) = &qualified_sum_out {
                                merged.insert(qout.clone(), format_computed_value(0.0));
                            }
                            joined_rows.push(merged);
                        }
                    } else {
                        let mut merged = row.clone();
                        if let Some(qc) = &qualified_count {
                            merged.insert(qc.clone(), matches.len().to_string());
                        }
                        if let (Some(qin), Some(qout)) = (&sum_in_q, &qualified_sum_out) {
                            let total: f64 = matches
                                .iter()
                                .filter_map(|m| m.get(qin.as_str()).and_then(|s| s.parse::<f64>().ok()))
                                .sum();
                            merged.insert(qout.clone(), format_computed_value(total));
                        }
                        joined_rows.push(merged);
                    }
                }
                None if join.kind == JoinKind::Left => {
                    let mut merged = row.clone();
                    if let Some(qc) = &qualified_count {
                        merged.insert(qc.clone(), "0".to_string());
                    }
                    if let Some(qout) = &qualified_sum_out {
                        merged.insert(qout.clone(), format_computed_value(0.0));
                    }
                    joined_rows.push(merged);
                }
                None => {}
            }

            if joined_rows.len() > JOIN_RESULT_LIMIT {
                bail!("multi-table query exceeded the in-memory join result limit ({})", JOIN_RESULT_LIMIT);
            }
        }
        return Ok(joined_rows);
    }

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

fn can_apply_early_window(
    resolved_filters: &[ResolvedFilter],
    resolved_filter_tree: Option<&ResolvedFilterTreeNode>,
    resolved_order_by: &[ResolvedOrderBy],
    resolved_aggregations: &[serde_json::Value],
    available_aliases: &HashSet<String>,
    remaining_joins: &[ResolvedJoin],
) -> bool {
    if resolved_order_by.is_empty() {
        return false;
    }
    if resolved_filter_tree.is_some() {
        return false;
    }
    if !resolved_aggregations.is_empty() {
        return false;
    }
    if !resolved_filters
        .iter()
        .all(|filter| available_aliases.contains(&filter.field.alias))
    {
        return false;
    }
    if !resolved_order_by
        .iter()
        .all(|order| available_aliases.contains(&order.field.alias))
    {
        return false;
    }
    remaining_joins.iter().all(|join| join.kind == JoinKind::Left)
}

fn apply_filter_tree(rows: Vec<FlatRow>, node: &ResolvedFilterTreeNode) -> Vec<FlatRow> {
    rows.into_iter()
        .filter(|row| evaluate_filter_tree(row, node))
        .collect()
}

fn evaluate_filter_tree(row: &FlatRow, node: &ResolvedFilterTreeNode) -> bool {
    match node {
        ResolvedFilterTreeNode::Condition(filter) => evaluate_filter(row, filter),
        ResolvedFilterTreeNode::Group { op, filters } => match op {
            LogicalOp::And => filters.iter().all(|child| evaluate_filter_tree(row, child)),
            LogicalOp::Or => filters.iter().any(|child| evaluate_filter_tree(row, child)),
        },
    }
}

fn evaluate_filter(row: &FlatRow, filter: &ResolvedFilter) -> bool {
    let Some(actual) = row.get(&filter.field.qualified) else {
        return false;
    };

    compare_filter_value(actual, filter.op, &filter.value, filter.value_to.as_deref(), &filter.value_options, filter.field_type)
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

fn enrich_rows_with_computed(rows: &mut [FlatRow], projections: &[Projection]) {
    for row in rows {
        let context = numeric_context_from_row(row);

        for projection in projections {
            if let ProjectionKind::Computed { qualified, expr, .. } = &projection.kind {
                row.insert(qualified.clone(), format_computed_value(evaluate(expr, &context)));
            }
        }
    }
}

fn format_computed_value(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{}", value as i64)
    } else {
        let formatted = format!("{:.6}", value);
        formatted.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn computed_qualified(header: &str) -> String {
    format!("__computed.{}", header)
}

fn numeric_context_from_row(row: &FlatRow) -> HashMap<String, f64> {
    let mut context = HashMap::new();
    for (field, value) in row {
        if let Ok(number) = value.parse::<f64>() {
            context.insert(field.clone(), number);
            if let Some((_, unqualified)) = field.split_once('.') {
                context.entry(unqualified.to_string()).or_insert(number);
            }
        }
    }
    context
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
                if let Some(field) = params.get("field").and_then(|v| v.as_str()) {
                    let qualified = parse_qualified_field(field, base_alias)?;
                    let count = rows.iter().filter(|row| row.get(&qualified.qualified).is_some()).count();
                    results.push(AggregationResult {
                        headers: vec![field.to_string(), "count".to_string()],
                        rows: vec![vec![field.to_string(), count.to_string()]],
                        summary: Some(count as f64),
                    });
                } else {
                    results.push(AggregationResult {
                        headers: vec![],
                        rows: vec![],
                        summary: Some(rows.len() as f64),
                    });
                }
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
                let having = resolve_having(params)?;
                let metric_rows = entries
                    .into_iter()
                    .map(|(value, count)| (value, count as f64))
                    .collect::<Vec<_>>();
                let metric_rows = apply_having(metric_rows, having.as_ref());
                results.push(AggregationResult {
                    headers: vec![field.to_string(), "count".to_string()],
                    rows: metric_rows
                        .into_iter()
                        .map(|(value, count)| vec![value, format_metric_value(count)])
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
                let having = resolve_having(params)?;

                for row in rows {
                    let context = numeric_context_from_row(row);
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
                    let entries = apply_having(entries, having.as_ref());
                    results.push(AggregationResult {
                        headers: vec![group_field.to_string(), "sum".to_string()],
                        rows: entries
                            .into_iter()
                            .map(|(group, sum)| vec![group, format_metric_value(sum)])
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
            "Avg" | "Min" | "Max" => {
                let group_by = params.get("group_by").and_then(|value| value.as_str());
                let expression = params
                    .get("expression")
                    .and_then(|value| value.as_str())
                    .or_else(|| params.get("field").and_then(|value| value.as_str()))
                    .ok_or_else(|| anyhow!("{} aggregation requires 'field' or 'expression'", agg_type))?;
                let parsed = parse_expression(expression)?;
                let having = resolve_having(params)?;

                if let Some(group_field) = group_by {
                    let group = parse_qualified_field(group_field, base_alias)?;
                    let mut grouped = HashMap::<String, Vec<f64>>::new();
                    for row in rows {
                        let context = numeric_context_from_row(row);
                        let value = evaluate(&parsed, &context);
                        let group_value = row.get(&group.qualified).cloned().unwrap_or_default();
                        grouped.entry(group_value).or_default().push(value);
                    }

                    let mut entries = grouped
                        .into_iter()
                        .map(|(group_value, values)| (group_value, aggregate_numeric_values(agg_type, &values)))
                        .collect::<Vec<_>>();
                    entries.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));
                    let entries = apply_having(entries, having.as_ref());
                    results.push(AggregationResult {
                        headers: vec![group_field.to_string(), agg_type.to_lowercase()],
                        rows: entries
                            .into_iter()
                            .map(|(group_value, metric)| vec![group_value, format_metric_value(metric)])
                            .collect(),
                        summary: Some(aggregate_numeric_values(agg_type, &rows.iter().map(|row| evaluate(&parsed, &numeric_context_from_row(row))).collect::<Vec<_>>())),
                    });
                } else {
                    let values = rows
                        .iter()
                        .map(|row| evaluate(&parsed, &numeric_context_from_row(row)))
                        .collect::<Vec<_>>();
                    results.push(AggregationResult {
                        headers: vec![],
                        rows: vec![],
                        summary: Some(aggregate_numeric_values(agg_type, &values)),
                    });
                }
            }
            "CountDistinct" => {
                let field = params
                    .get("field")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| anyhow!("CountDistinct aggregation requires 'field'"))?;
                let qualified = parse_qualified_field(field, base_alias)?;
                let group_by = params.get("group_by").and_then(|value| value.as_str());
                let having = resolve_having(params)?;

                if let Some(group_field) = group_by {
                    let group = parse_qualified_field(group_field, base_alias)?;
                    let mut grouped = HashMap::<String, HashSet<String>>::new();
                    for row in rows {
                        let group_value = row.get(&group.qualified).cloned().unwrap_or_default();
                        let distinct_value = row.get(&qualified.qualified).cloned().unwrap_or_default();
                        grouped.entry(group_value).or_default().insert(distinct_value);
                    }

                    let mut entries = grouped
                        .into_iter()
                        .map(|(group_value, values)| (group_value, values.len() as f64))
                        .collect::<Vec<_>>();
                    entries.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));
                    let total = entries.iter().map(|(_, count)| *count).sum::<f64>();
                    let entries = apply_having(entries, having.as_ref());
                    results.push(AggregationResult {
                        headers: vec![group_field.to_string(), "count_distinct".to_string()],
                        rows: entries
                            .into_iter()
                            .map(|(group_value, count)| vec![group_value, format_metric_value(count)])
                            .collect(),
                        summary: Some(total),
                    });
                } else {
                    let count = rows
                        .iter()
                        .filter_map(|row| row.get(&qualified.qualified).cloned())
                        .collect::<HashSet<_>>()
                        .len() as f64;
                    results.push(AggregationResult {
                        headers: vec![],
                        rows: vec![],
                        summary: Some(count),
                    });
                }
            }
            other => bail!("unsupported aggregation '{}' for multi-table query", other),
        }
    }

    Ok(results)
}

fn aggregate_numeric_values(kind: &str, values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    match kind {
        "Avg" => values.iter().sum::<f64>() / values.len() as f64,
        "Min" => values.iter().copied().reduce(f64::min).unwrap_or(0.0),
        "Max" => values.iter().copied().reduce(f64::max).unwrap_or(0.0),
        _ => 0.0,
    }
}

fn format_metric_value(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{}", value as i64)
    } else {
        format!("{:.2}", value)
    }
}

fn resolve_having(params: &serde_json::Value) -> Result<Option<ResolvedHaving>> {
    let Some(having) = params.get("having").and_then(|value| value.as_object()) else {
        return Ok(None);
    };

    let op = having
        .get("op")
        .and_then(|value| value.as_str())
        .map(ComparisonOp::from_str)
        .unwrap_or(ComparisonOp::Eq);
    let value = having
        .get("value")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let value_to = having.get("value_to").and_then(|value| value.as_str()).map(str::to_string);
    let value_options = having
        .get("values")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(Some(ResolvedHaving { op, value, value_to, value_options }))
}

fn apply_having(entries: Vec<(String, f64)>, having: Option<&ResolvedHaving>) -> Vec<(String, f64)> {
    match having {
        Some(having) => entries
            .into_iter()
            .filter(|(_, metric)| {
                compare_filter_value(
                    &metric.to_string(),
                    having.op,
                    &having.value,
                    having.value_to.as_deref(),
                    &having.value_options,
                    Some(FieldType::Float),
                )
            })
            .collect(),
        None => entries,
    }
}

fn resolve_param(raw: &str, params_map: &HashMap<String, String>) -> Result<String> {
    if let Some(spec) = raw.strip_prefix('$') {
        let mut parts = spec.split('|');
        let key = parts.next().unwrap_or_default();
        let mut value = params_map
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("missing param '{}'", key))?;

        for transform in parts {
            if transform.eq_ignore_ascii_case("trim") {
                value = value.trim().to_string();
                continue;
            }

            if transform.eq_ignore_ascii_case("lowercase") {
                value = value.to_lowercase();
                continue;
            }

            if transform.eq_ignore_ascii_case("uppercase") {
                value = value.to_uppercase();
                continue;
            }

            if let Some(split_spec) = transform.strip_prefix("split:") {
                let mut split_parts = split_spec.splitn(2, ':');
                let delimiter = split_parts.next().unwrap_or_default();
                if delimiter.is_empty() {
                    return Err(anyhow!("invalid split transform '{}': empty delimiter", transform));
                }
                let index_raw = split_parts
                    .next()
                    .ok_or_else(|| anyhow!("invalid split transform '{}': missing index", transform))?;
                let index = index_raw
                    .parse::<usize>()
                    .map_err(|_| anyhow!("invalid split index '{}' in '{}'", index_raw, transform))?;
                let segments: Vec<&str> = value.split(delimiter).collect();
                value = segments
                    .get(index)
                    .map(|segment| segment.to_string())
                    .ok_or_else(|| anyhow!("split transform '{}' out of bounds for value '{}'", transform, value))?;
                continue;
            }

            return Err(anyhow!("unsupported param transform '{}'", transform));
        }

        Ok(value)
    } else {
        Ok(raw.to_string())
    }
}

fn resolve_optional_param(raw: Option<&str>, params_map: &HashMap<String, String>) -> Result<Option<String>> {
    raw.map(|value| resolve_param(value, params_map)).transpose()
}

fn resolve_param_list(values: &[String], params_map: &HashMap<String, String>) -> Vec<String> {
    values
        .iter()
        .map(|raw| {
            if let Some(key) = raw.strip_prefix('$') {
                params_map.get(key).cloned().unwrap_or_else(|| raw.clone())
            } else {
                raw.clone()
            }
        })
        .collect()
}

fn parse_logical_op(value: &str) -> LogicalOp {
    if value.eq_ignore_ascii_case("Or") {
        LogicalOp::Or
    } else {
        LogicalOp::And
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

/// Aligns join keys across tables/column types (e.g. `6351` vs `6351.0`, whitespace).
fn normalize_join_key_fragment(value: &str) -> String {
    let t = value.trim();
    if t.is_empty() {
        return String::new();
    }
    if let Ok(n) = t.parse::<f64>() {
        if (n.fract()).abs() < f64::EPSILON && n.is_finite() {
            return format!("{}", n as i64);
        }
    }
    t.to_string()
}

fn compose_key<'a>(row: &FlatRow, fields: impl Iterator<Item = &'a QualifiedField>) -> Option<String> {
    let mut values = Vec::new();
    for field in fields {
        let raw = row.get(&field.qualified)?.clone();
        values.push(normalize_join_key_fragment(&raw));
    }
    Some(values.join(&JOIN_SEPARATOR.to_string()))
}

/// Stable fingerprint of a fetched join row (all qualified columns).
fn flat_row_fingerprint(row: &FlatRow) -> String {
    let mut keys: Vec<_> = row.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|key| format!("{key}={}", row.get(key).map(|s| s.as_str()).unwrap_or("")))
        .collect::<Vec<_>>()
        .join(&JOIN_SEPARATOR.to_string())
}

fn qualify(alias: &str, field: &str) -> String {
    format!("{}.{}", alias, field)
}