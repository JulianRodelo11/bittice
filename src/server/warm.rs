//! Background cache warm for saved-query mirror tables.
//!
//! Phase P0 (filters): index fields used in HTTP query params — highest ROI, runs first.
//! Phase P1 (extended): select/order/join keys — skipped for huge tables and explicit denylist.
//!
//! Env:
//! - `BITTICE_WARM_SKIP_TABLES` — comma-separated `entity/table` keys to never background-warm
//! - `BITTICE_WARM_MAX_TABLE_MB` — skip P1 when mirror dir exceeds this size (default 500; 0 = off)
//! - `BITTICE_WARM_MAINTENANCE_SECS` — interval between maintenance P0 passes (default 300)

use crate::core::saved_queries::{SavedOperation, SavedQuery};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

use super::ServerState;

type TableKey = (String, String);
type FieldSet = HashSet<String>;
type WarmTargets = HashMap<TableKey, FieldSet>;

const DEFAULT_MAX_TABLE_MB: u64 = 500;
const DEFAULT_MAINTENANCE_SECS: u64 = 300;
const P1_YIELD_MS: u64 = 2_000;

#[derive(Clone, Debug)]
pub struct WarmConfig {
    pub skip_tables: HashSet<String>,
    /// When > 0, P1 skips tables whose on-disk mirror exceeds this many megabytes.
    pub max_table_mb: u64,
    pub maintenance_secs: u64,
}

impl WarmConfig {
    pub fn from_env() -> Self {
        let skip_tables = std::env::var("BITTICE_WARM_SKIP_TABLES")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let max_table_mb = std::env::var("BITTICE_WARM_MAX_TABLE_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_TABLE_MB);

        let maintenance_secs = std::env::var("BITTICE_WARM_MAINTENANCE_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(DEFAULT_MAINTENANCE_SECS);

        Self {
            skip_tables,
            max_table_mb,
            maintenance_secs,
        }
    }

    fn table_key(entity: &str, table: &str) -> String {
        format!("{}/{}", entity, table)
    }

    pub fn is_denied(&self, entity: &str, table: &str) -> bool {
        self.skip_tables
            .contains(&Self::table_key(entity, table))
    }

    pub fn exceeds_size_limit(&self, entity: &str, table: &str) -> bool {
        if self.max_table_mb == 0 {
            return false;
        }
        let bytes = table_mirror_bytes(entity, table);
        bytes > self.max_table_mb.saturating_mul(1024 * 1024)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WarmPhase {
    /// Filter fields from saved ops — always run, not subject to size skip.
    Filters,
    /// Select/order/join fields — subject to skip list and size limit.
    Extended,
}

#[derive(Clone, Copy, Debug)]
struct WarmOpts {
    phase: WarmPhase,
    /// P1 maintenance: only touch tables already in the open-table cache.
    only_if_open: bool,
}

/// Spawn the background warm loop (P0 immediately, P1 after a short yield, then periodic P0).
pub fn spawn_background_warm(state: Arc<ServerState>) {
    tokio::spawn(async move {
        let cfg = WarmConfig::from_env();
        if !cfg.skip_tables.is_empty() || cfg.max_table_mb > 0 {
            info!(
                "Warm: skip_tables={} max_table_mb={} maintenance_secs={}",
                cfg.skip_tables.len(),
                cfg.max_table_mb,
                cfg.maintenance_secs
            );
        }

        let mut maintenance = false;
        loop {
            let cfg = WarmConfig::from_env();
            let warmed = run_warm_cycle(state.clone(), &cfg, maintenance).await;
            if warmed > 0 {
                debug!(
                    "Warm: {} table(s) in {} pass",
                    warmed,
                    if maintenance { "maintenance" } else { "startup" }
                );
            }

            if !maintenance {
                maintenance = true;
                tokio::time::sleep(Duration::from_millis(P1_YIELD_MS)).await;
                let p1 = run_warm_phase(
                    state.clone(),
                    &cfg,
                    WarmOpts {
                        phase: WarmPhase::Extended,
                        only_if_open: false,
                    },
                )
                .await;
                if p1 > 0 {
                    debug!("Warm: P1 extended warmed {} table(s) on startup", p1);
                }
            }

            tokio::time::sleep(Duration::from_secs(cfg.maintenance_secs)).await;
        }
    });
}

async fn run_warm_cycle(state: Arc<ServerState>, cfg: &WarmConfig, maintenance: bool) -> usize {
    let p0 = run_warm_phase(
        state.clone(),
        cfg,
        WarmOpts {
            phase: WarmPhase::Filters,
            only_if_open: false,
        },
    )
    .await;
    let p1 = if maintenance {
        run_warm_phase(
            state,
            cfg,
            WarmOpts {
                phase: WarmPhase::Extended,
                only_if_open: true,
            },
        )
        .await
    } else {
        0
    };
    p0 + p1
}

async fn run_warm_phase(state: Arc<ServerState>, cfg: &WarmConfig, opts: WarmOpts) -> usize {
    let Ok(ops) =
        crate::core::saved_queries::load_operations_with_filter(state.entity_filter.clone())
    else {
        return 0;
    };

    let (filter_targets, extended_targets) = collect_warm_plans(&ops);
    let targets = match opts.phase {
        WarmPhase::Filters => filter_targets,
        WarmPhase::Extended => extended_targets,
    };

    if targets.is_empty() {
        return 0;
    }

    let tm = state.table_manager.clone();
    let filtered: Vec<(TableKey, Vec<String>)> = targets
        .into_iter()
        .filter(|((entity, table), _)| {
            if cfg.is_denied(entity, table) {
                return false;
            }
            if opts.phase == WarmPhase::Extended && cfg.exceeds_size_limit(entity, table) {
                debug!(
                    "Warm: skip P1 {}/{} — mirror exceeds {} MB limit",
                    entity, table, cfg.max_table_mb
                );
                return false;
            }
            if opts.only_if_open && !tm.is_table_open(entity, table) {
                return false;
            }
            true
        })
        .map(|(k, fields)| (k, fields.into_iter().collect()))
        .collect();

    if filtered.is_empty() {
        return 0;
    }

    tokio::task::spawn_blocking(move || {
        let mut warmed_count = 0usize;
        for ((entity, table_name), fields) in filtered {
            if let Ok(table_lock) = tm.get_table(&entity, &table_name) {
                let table = table_lock.read().unwrap();
                let _ = table.warm_up(&fields);
                warmed_count += 1;
            }
        }
        warmed_count
    })
    .await
    .unwrap_or(0)
}

fn collect_warm_plans(ops: &[SavedOperation]) -> (WarmTargets, WarmTargets) {
    let mut filter_targets: WarmTargets = HashMap::new();
    let mut extended_targets: WarmTargets = WarmTargets::new();

    for op in ops {
        if let SavedOperation::Read(q) = op {
            collect_filter_fields(q, &mut filter_targets);
            collect_extended_fields(q, &mut extended_targets);
        }
    }

    (filter_targets, extended_targets)
}

fn collect_filter_fields(q: &SavedQuery, targets: &mut WarmTargets) {
    let base_alias = q.base_alias();
    for f in &q.filters {
        if f.field == "?" {
            continue;
        }
        if let Some((entity, table, field)) = resolve_field_table(q, &base_alias, &f.field) {
            targets.entry((entity, table)).or_default().insert(field);
        }
    }
}

fn collect_extended_fields(q: &SavedQuery, targets: &mut WarmTargets) {
    let base_alias = q.base_alias();

    for f in &q.selected_fields {
        if f == "*" {
            continue;
        }
        if let Some((alias, field)) = split_alias_field(f, &base_alias) {
            if alias == base_alias {
                targets
                    .entry((q.entity.clone(), q.table.clone()))
                    .or_default()
                    .insert(field);
            }
        }
    }
    for s in &q.select {
        if let Some((alias, field)) = split_alias_field(&s.field, &base_alias) {
            if alias == base_alias {
                targets
                    .entry((q.entity.clone(), q.table.clone()))
                    .or_default()
                    .insert(field);
            }
        }
    }
    for o in &q.order_by {
        if let Some((entity, table, field)) = resolve_field_table(q, &base_alias, &o.field) {
            targets.entry((entity, table)).or_default().insert(field);
        }
    }
    for join in &q.joins {
        let join_alias = join.alias.as_deref().unwrap_or(join.table.as_str()).to_string();
        let join_entity = join.entity.clone().unwrap_or_else(|| q.entity.clone());
        let join_entry = targets
            .entry((join_entity, join.table.clone()))
            .or_default();
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

fn resolve_field_table(
    q: &SavedQuery,
    base_alias: &str,
    field_ref: &str,
) -> Option<(String, String, String)> {
    let (alias, field) = split_alias_field(field_ref, base_alias)?;
    if alias == base_alias {
        return Some((q.entity.clone(), q.table.clone(), field));
    }
    q.joins
        .iter()
        .find(|join| join.alias.as_deref().unwrap_or(join.table.as_str()) == alias)
        .map(|join| {
            (
                join.entity.clone().unwrap_or_else(|| q.entity.clone()),
                join.table.clone(),
                field,
            )
        })
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

fn table_mirror_bytes(entity: &str, table: &str) -> u64 {
    let path = crate::core::data_paths::mirror_entity_dir(entity).join(table);
    dir_size_bytes(&path).unwrap_or(0)
}

fn dir_size_bytes(path: &Path) -> std::io::Result<u64> {
    if !path.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let p = entry.path();
            if ty.is_dir() {
                stack.push(p);
            } else if ty.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::saved_queries::{SavedFilter, SavedJoin, SavedJoinCondition, SavedOperation, SavedOrderBy, SavedQuery};

    fn sample_plate_query() -> SavedQuery {
        SavedQuery {
            name: "beparking-transaction-plate".into(),
            entity: "db_attendant_prod".into(),
            table: "entradaVehiculos".into(),
            table_alias: Some("ev".into()),
            joins: vec![SavedJoin {
                join_type: "Left".into(),
                entity: Some("smartinvoicing".into()),
                table: "Factura_electronica_ParkingGo".into(),
                alias: Some("sfi".into()),
                on: vec![SavedJoinCondition {
                    left: "tr.transaccionId".into(),
                    op: "Eq".into(),
                    right: "sfi.transaccionId".into(),
                }],
                count_matches_as: None,
                sum_matches_field: None,
                sum_matches_as: None,
            }],
            filters: vec![SavedFilter {
                field: "ev.placa".into(),
                op: "Eq".into(),
                value: "$placa".into(),
                value_to: None,
                values: vec![],
                field_type: None,
            }],
            filter_tree: None,
            filters_op: "And".into(),
            aggregations: vec![],
            order_by: vec![SavedOrderBy {
                field: "ev.fechaHoraIngreso".into(),
                direction: "Desc".into(),
            }],
            limit: Some(100),
            limit_param: None,
            selected_fields: vec![],
            select: vec![],
            response_grouping: None,
            auth_config: None,
            execution_profile: None,
        }
    }

    #[test]
    fn filter_plan_targets_placa_on_base_table() {
        let op = SavedOperation::Read(sample_plate_query());
        let (filters, extended) = collect_warm_plans(&[op]);
        assert_eq!(
            filters.get(&("db_attendant_prod".into(), "entradaVehiculos".into())),
            Some(&HashSet::from(["placa".into()]))
        );
        assert!(extended.contains_key(&(
            "smartinvoicing".into(),
            "Factura_electronica_ParkingGo".into()
        )));
    }

    #[test]
    fn size_limit_skips_when_mirror_exceeds_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let table_dir = dir.path().join("big_table");
        std::fs::create_dir_all(&table_dir).unwrap();
        std::fs::write(table_dir.join("data.dat"), vec![0u8; 2 * 1024 * 1024]).unwrap();

        let bytes = dir_size_bytes(&table_dir).unwrap();
        let max_table_mb = 1u64;
        assert!(bytes > max_table_mb.saturating_mul(1024 * 1024));
    }

    #[test]
    fn denylist_matches_entity_table_key() {
        let cfg = WarmConfig {
            skip_tables: HashSet::from(["smartinvoicing/Factura_electronica_ParkingGo".into()]),
            max_table_mb: 0,
            maintenance_secs: 300,
        };
        assert!(cfg.is_denied("smartinvoicing", "Factura_electronica_ParkingGo"));
        assert!(!cfg.is_denied("db_attendant_prod", "entradaVehiculos"));
    }
}
