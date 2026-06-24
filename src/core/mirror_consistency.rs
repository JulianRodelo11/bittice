//! Compare MySQL row counts with Bittice mirror live rows (local drift check).
//!
//! Used by `bittice check-mirror` and `server::self_health`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use serde::Serialize;

/// Tables where COUNT(*) drift is expected (append-only audit tables).
pub const AUDIT_DENYLIST: &[&str] = &[
    "bittice.consistency_checks",
    "bittice.drift_incidents",
    "bittice.drift_diagnostics",
    "bittice.schema_migrations",
];

#[derive(Debug, Clone, Serialize)]
pub struct TableConsistencyRow {
    pub profile: String,
    pub table: String,
    pub source_count: u64,
    pub mirror_count: u64,
    /// MySQL rows minus mirror live rows (positive = mirror behind or missing rows).
    pub diff: i64,
    pub ok: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CheckMirrorOptions {
    pub entity_filter: Option<String>,
    pub table_filter: Option<String>,
    /// Re-read counts after 2s when the first pass shows drift (absorbs CDC lag races).
    pub revalidate: bool,
}

struct ProfileEntry {
    entity_folder: String,
    config_path: PathBuf,
    state_path: PathBuf,
}

pub async fn check_mirror_consistency(opts: CheckMirrorOptions) -> Result<Vec<TableConsistencyRow>> {
    let data_root = crate::core::data_paths::resolved_data_root();
    let profiles = discover_profiles(&data_root);
    if profiles.is_empty() {
        anyhow::bail!(
            "no CDC profiles under {}/profiles — run Connect and sync first",
            data_root.display()
        );
    }

    let entity_filter = opts
        .entity_filter
        .as_ref()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty());

    let table_filter = opts
        .table_filter
        .as_ref()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    let mut rows = Vec::new();

    for profile in profiles {
        if let Some(ref filter) = entity_filter {
            let folder = profile.entity_folder.to_lowercase();
            let cfg_entity = read_json(&profile.config_path)
                .ok()
                .and_then(|j| j.get("entity").and_then(|v| v.as_str()).map(str::to_string))
                .unwrap_or_else(|| profile.entity_folder.clone())
                .to_lowercase();
            if folder != *filter && cfg_entity != *filter {
                continue;
            }
        }

        let cfg_json = match read_json(&profile.config_path) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("check-mirror: skip profile {}: {e:#}", profile.entity_folder);
                continue;
            }
        };
        let state_json = match read_json(&profile.state_path) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let bootstrapped: Vec<String> = state_json
            .get("bootstrapped_tables")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if bootstrapped.is_empty() {
            continue;
        }

        let sync_all = cfg_json
            .get("sync_all_databases")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let database = cfg_json
            .get("database")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let entity = cfg_json
            .get("entity")
            .and_then(|v| v.as_str())
            .unwrap_or(&profile.entity_folder)
            .to_string();

        let mut conn = connect_source(&cfg_json).await.with_context(|| {
            format!(
                "MySQL connect failed for profile '{}'",
                profile.entity_folder
            )
        })?;

        for qkey in &bootstrapped {
            if AUDIT_DENYLIST.contains(&qkey.as_str()) {
                continue;
            }
            if let Some(ref tf) = table_filter {
                if !qkey.eq_ignore_ascii_case(tf) {
                    continue;
                }
            }

            let (schema, table_sql) = parse_qkey(sync_all, &database, qkey);
            let disk_entity = if sync_all {
                schema.to_lowercase()
            } else {
                entity.clone()
            };
            let mirror_dir = resolve_mirror_dir(&data_root, &disk_entity, &table_sql);

            let mut mirror_count = mirror_live_count(&mirror_dir).unwrap_or(0);
            let mut source_count =
                mysql_count(&mut conn, sync_all, &database, &schema, &table_sql).await?;
            let mut diff = source_count as i64 - mirror_count as i64;

            if diff != 0 && opts.revalidate {
                tokio::time::sleep(Duration::from_secs(2)).await;
                mirror_count = mirror_live_count(&mirror_dir).unwrap_or(mirror_count);
                source_count =
                    mysql_count(&mut conn, sync_all, &database, &schema, &table_sql).await?;
                diff = source_count as i64 - mirror_count as i64;
            }

            rows.push(TableConsistencyRow {
                profile: profile.entity_folder.clone(),
                table: qkey.clone(),
                source_count,
                mirror_count,
                diff,
                ok: diff == 0,
            });
        }

        let _ = conn.disconnect().await;
    }

    rows.sort_by(|a, b| a.table.cmp(&b.table));
    Ok(rows)
}

fn discover_profiles(data_root: &Path) -> Vec<ProfileEntry> {
    let mut out = Vec::new();
    let profiles_dir = data_root.join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir = entry.path();
        let cfg = dir.join("cdc_config.json");
        if !cfg.is_file() {
            continue;
        }
        out.push(ProfileEntry {
            entity_folder: entry.file_name().to_string_lossy().into_owned(),
            config_path: cfg,
            state_path: dir.join("cdc_state.json"),
        });
    }
    out.sort_by(|a, b| a.entity_folder.cmp(&b.entity_folder));
    out
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

pub fn parse_qkey(sync_all: bool, database: &str, qkey: &str) -> (String, String) {
    if sync_all {
        if let Some((s, t)) = qkey.split_once('.') {
            return (s.to_string(), t.to_string());
        }
    }
    (database.to_string(), qkey.to_string())
}

pub fn resolve_mirror_dir(data_root: &Path, entity: &str, table: &str) -> PathBuf {
    let primary = data_root.join("mirror").join(entity);
    let direct = primary.join(table);
    if direct.is_dir() {
        return direct;
    }
    if primary.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&primary) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && e.file_name().to_string_lossy().eq_ignore_ascii_case(table)
                {
                    return e.path();
                }
            }
        }
    }
    let legacy = data_root.join(entity).join(table);
    if legacy.is_dir() {
        return legacy;
    }
    direct
}

/// Live row count from segment offsets and deleted bitmaps (same logic as self_health).
pub fn mirror_live_count(table_dir: &Path) -> Option<u64> {
    let manifest = table_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest).ok()?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let pk_field = val
        .get("primary_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut seg_ids: Vec<u64> = val
        .get("segments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("id").and_then(|v| v.as_u64()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(active_id) = val.get("active_segment_id").and_then(|v| v.as_u64()) {
        if !seg_ids.contains(&active_id) {
            seg_ids.push(active_id);
        }
    }

    let segments_dir = table_dir.join("segments");
    let mut live: i64 = 0;
    for seg_id in seg_ids {
        let seg_path = segments_dir.join(format!("seg_{:04}", seg_id));
        let offsets_path = match pk_field.as_deref() {
            Some(name) => seg_path.join(format!("{}.offsets", name)),
            None => match std::fs::read_dir(&seg_path).ok().and_then(|entries| {
                entries.flatten().find_map(|e| {
                    if e.file_name().to_string_lossy().ends_with(".offsets") {
                        Some(e.path())
                    } else {
                        None
                    }
                })
            }) {
                Some(p) => p,
                None => continue,
            },
        };
        let rc = match std::fs::metadata(&offsets_path) {
            Ok(m) => (m.len() / 8) as i64,
            Err(_) => continue,
        };
        let dc = match std::fs::File::open(seg_path.join("deleted.bitmap")) {
            Ok(file) => roaring::RoaringBitmap::deserialize_from(file)
                .map(|bm| bm.len() as i64)
                .unwrap_or(0),
            Err(_) => 0,
        };
        live += (rc - dc).max(0);
    }
    Some(live as u64)
}

pub async fn connect_source(cfg_json: &serde_json::Value) -> Result<Conn> {
    let host = cfg_json
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let port = cfg_json
        .get("port")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .or_else(|| cfg_json.get("port").and_then(|v| v.as_u64()).map(|p| p as u16))
        .unwrap_or(3306);
    let user = cfg_json
        .get("user")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let pass = cfg_json
        .get("pass")
        .or_else(|| cfg_json.get("password"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if host.is_empty() || user.is_empty() {
        anyhow::bail!("cdc_config.json missing host/user");
    }

    let mut builder = OptsBuilder::default()
        .ip_or_hostname(host.clone())
        .tcp_port(port)
        .user(Some(user))
        .pass(Some(pass));

    if host.contains("rds.amazonaws.com") || host.ends_with(".amazonaws.com") {
        builder = builder.ssl_opts(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        );
    }

    let opts: Opts = builder.into();
    Conn::new(opts)
        .await
        .with_context(|| format!("connect to {host}"))
}

/// Sync-all qkeys use lowercased `schema.table`; MySQL may use mixed-case schema names
/// (e.g. `SmartInvoicing` vs `smartinvoicing` on Linux with `lower_case_table_names=0`).
pub async fn resolve_mysql_schema(conn: &mut Conn, schema_hint: &str) -> Result<String> {
    let rows: Vec<(String,)> = conn
        .exec(
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
             WHERE LOWER(SCHEMA_NAME) = LOWER(?) ORDER BY SCHEMA_NAME LIMIT 2",
            (schema_hint,),
        )
        .await
        .context("resolve schema via information_schema.SCHEMATA")?;
    match rows.len() {
        0 => anyhow::bail!(
            "no schema matching `{schema_hint}` (case-insensitive) on server"
        ),
        1 => Ok(rows.into_iter().next().unwrap().0),
        _ => anyhow::bail!(
            "ambiguous schema `{schema_hint}`: multiple databases match case-insensitively"
        ),
    }
}

pub async fn mysql_count(
    conn: &mut Conn,
    sync_all: bool,
    database: &str,
    schema: &str,
    table: &str,
) -> Result<u64> {
    let ident = |s: &str| s.replace('`', "``");
    let sql = if sync_all {
        let schema_mysql = resolve_mysql_schema(conn, schema).await?;
        format!(
            "SELECT COUNT(*) FROM `{}`.`{}`",
            ident(&schema_mysql),
            ident(table)
        )
    } else {
        let use_sql = format!("USE `{}`", ident(database));
        conn.query_drop(use_sql).await?;
        format!("SELECT COUNT(*) FROM `{}`", ident(table))
    };
    let n: u64 = conn
        .query_first(sql)
        .await?
        .ok_or_else(|| anyhow::anyhow!("COUNT returned no row"))?;
    Ok(n)
}
