//! Engine-side self-health task. Replaces the external Python cron at
//! `deploy/ops/consistency_check_reporter.py` so customers no longer need to
//! install any sidecar scripts on their VM.
//!
//! What it does (every `self_health_interval_secs`):
//!   1. Iterate every `data/profiles/<entity>/cdc_state.json`
//!   2. For each bootstrapped table (minus the audit denylist and the
//!      per-deployment watch lists from the control plane):
//!        - COUNT(*) on source via mysql_async (one connection per profile,
//!          per tick — same shape as the Python v2 to avoid host_cache 1129)
//!        - Sum (record_count − deleted_count) across mirror segments
//!   3. POST /v1/health/consistency-check with the batch
//!   4. For each table with diff != 0, capture a diagnostic snapshot
//!      (engine version, CDC state, source MySQL identity, mirror health)
//!      and POST /v1/health/incident-with-diagnostics
//!
//! Behavior is 100 % driven by the control plane (`/v1/config`). The only
//! local env vars consulted are the three identity vars also used by
//! `heartbeat.rs` — DEPLOYMENT_ID, INSTANCE_TOKEN, CONTROL_PLANE_URL.
//!
//! Auto-repair (fase 3) is not wired here yet; the config field is read and
//! forwarded as `auto_repair_attempted=false` in diagnostics until then.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use tracing::{debug, info, warn};

use crate::core::control_plane::{
    fetch_engine_config, post_consistency_check, post_incident_diagnostics, CdcDiagnostics,
    ConfigFetch, ConsistencyCheckRequest, DriftDiagnosticsRequest, EffectiveEngineConfig,
    EngineConfigResponse, MirrorDiagnostics, SourceDiagnostics, TableConsistency,
    TimingDiagnostics,
};

// 300s, not 60s. The control plane runs on Lambda; every poll is a paid
// invocation + 2 RDS SELECTs. Config changes here (self_health_enabled,
// watch lists, auto_repair toggles) are operational toggles, not
// urgencies — propagation in up to 5 min is fine. Cuts request load on
// the control plane by 12x. ETag/304 means even when nothing changed the
// roundtrip is cheap, but the Lambda still has to wake, query, and
// return — so cadence dominates the cost.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(300);
const CONFIG_INITIAL_BACKOFF_STEPS: &[u64] = &[5, 15, 45, 120, 300];

// Audit / append-only tables that always drift when compared with COUNT(*).
// Hardcoded as a built-in safety net — comparing `consistency_checks` to itself
// is meaningless (every cron INSERT adds RDS rows the CDC has yet to copy).
const BUILT_IN_AUDIT_DENYLIST: &[&str] = &[
    "bittice.consistency_checks",
    "bittice.drift_incidents",
    "bittice.drift_diagnostics",
    "bittice.schema_migrations",
];

/// Spawn the self-health loop. Returns immediately. No-op when identity env
/// vars are missing (= local mode, exactly like `heartbeat.rs`).
pub fn spawn_if_configured() {
    let Some(identity) = Identity::from_env() else {
        debug!("self_health: identity env vars not set — local mode, disabled.");
        return;
    };

    tokio::spawn(async move {
        run(identity).await;
    });
}

#[derive(Clone)]
struct Identity {
    deployment_id: String,
    instance_token: String,
    control_plane_url: String,
}

impl Identity {
    fn from_env() -> Option<Self> {
        let dep = std::env::var("BITTICE_DEPLOYMENT_ID").ok()?;
        let tok = std::env::var("BITTICE_INSTANCE_TOKEN").ok()?;
        let url = std::env::var("BITTICE_CONTROL_PLANE_URL").ok()?;
        let (dep, tok, url) = (dep.trim(), tok.trim(), url.trim());
        if dep.is_empty() || tok.is_empty() || url.is_empty() {
            return None;
        }
        Some(Self {
            deployment_id: dep.to_string(),
            instance_token: tok.to_string(),
            control_plane_url: url.trim_end_matches('/').to_string(),
        })
    }
}

async fn run(identity: Identity) {
    // 1. Block until we get the first config. The engine carries no defaults
    //    of its own — if the control plane is unreachable we wait, we don't
    //    decide.
    let mut current = match initial_config(&identity).await {
        Ok(c) => c,
        Err(e) => {
            warn!("self_health: gave up waiting for initial config: {e:#}");
            return;
        }
    };
    info!(
        "self_health: started (config_version={}, interval={}s, enabled={}).",
        current.config_version,
        current.effective_config.self_health_interval_secs,
        current.effective_config.self_health_enabled,
    );

    // Two independent cadences:
    //   - Config refresh:  every CONFIG_POLL_INTERVAL (300s). Each call hits
    //                      the control plane Lambda + 2 RDS SELECTs, so we
    //                      keep it slow. Operational toggles propagate within
    //                      ~5 min — fine for self_health_enabled/watchlists.
    //   - Drift check:     every effective_config.self_health_interval_secs.
    //                      Decoupled from the config refresh so a 60s check
    //                      interval is still honored (the loop ticks at 30s
    //                      to give 30s scheduling resolution to whichever
    //                      timer fires next).
    let mut last_config_fetch = std::time::Instant::now();
    let mut last_check = std::time::Instant::now()
        .checked_sub(Duration::from_secs(
            current.effective_config.self_health_interval_secs,
        ))
        .unwrap_or_else(std::time::Instant::now);
    let tick = Duration::from_secs(30);

    loop {
        tokio::time::sleep(tick).await;

        // Refresh config only when CONFIG_POLL_INTERVAL has elapsed.
        if last_config_fetch.elapsed() >= CONFIG_POLL_INTERVAL {
            match fetch_engine_config(
                &identity.control_plane_url,
                &identity.deployment_id,
                &identity.instance_token,
                Some(&current.config_version),
            )
            .await
            {
                Ok(ConfigFetch::Fresh(new_cfg)) => {
                    info!(
                        "self_health: config updated (version {} → {})",
                        current.config_version, new_cfg.config_version
                    );
                    current = new_cfg;
                }
                Ok(ConfigFetch::NotModified) => {}
                Err(e) => debug!("self_health: config refresh failed: {e:#}"),
            }
            last_config_fetch = std::time::Instant::now();
        }

        let cfg = &current.effective_config;
        if !cfg.self_health_enabled {
            continue;
        }
        let interval = Duration::from_secs(cfg.self_health_interval_secs.max(60));
        if last_check.elapsed() < interval {
            continue;
        }
        last_check = std::time::Instant::now();

        let data_root = crate::core::data_paths::resolved_data_root();
        if let Err(e) = run_check(&identity, cfg, &data_root).await {
            warn!("self_health: check tick failed: {e:#}");
        }
    }
}

/// Try to fetch the very first config. Retries with bounded exponential
/// backoff. Returns Err only when the steps are exhausted — in practice
/// CONFIG_INITIAL_BACKOFF_STEPS sums to ~8 minutes, after which we hand
/// control back to the caller (which logs and exits the task).
async fn initial_config(identity: &Identity) -> Result<EngineConfigResponse> {
    for (i, secs) in CONFIG_INITIAL_BACKOFF_STEPS.iter().enumerate() {
        match fetch_engine_config(
            &identity.control_plane_url,
            &identity.deployment_id,
            &identity.instance_token,
            None,
        )
        .await
        {
            Ok(ConfigFetch::Fresh(c)) => return Ok(c),
            Ok(ConfigFetch::NotModified) => unreachable!("no If-None-Match was sent"),
            Err(e) => {
                warn!(
                    "self_health: initial config fetch attempt {}/{} failed: {e:#}",
                    i + 1,
                    CONFIG_INITIAL_BACKOFF_STEPS.len()
                );
                tokio::time::sleep(Duration::from_secs(*secs)).await;
            }
        }
    }
    Err(anyhow!("control plane unreachable after backoff"))
}

// ── one tick ────────────────────────────────────────────────────────────────

async fn run_check(
    identity: &Identity,
    cfg: &EffectiveEngineConfig,
    data_root: &Path,
) -> Result<()> {
    let profiles = discover_profiles(data_root);
    if profiles.is_empty() {
        debug!("self_health: no profiles under {}/profiles — nothing to check.", data_root.display());
        return Ok(());
    }

    let mut rows: Vec<TableConsistency> = Vec::new();
    let mut drifts: Vec<DriftCapture> = Vec::new();
    let checked_at = chrono::Utc::now();

    for profile in profiles {
        // Load the JSON files. Same loose typing as the existing engine code.
        let cfg_json: serde_json::Value = match read_json(&profile.config_path) {
            Ok(v) => v,
            Err(e) => {
                warn!("self_health: skip profile {}: {e:#}", profile.entity_folder);
                continue;
            }
        };
        let state_json: serde_json::Value = match read_json(&profile.state_path) {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    "self_health: profile {} has no state yet: {e:#}",
                    profile.entity_folder
                );
                continue;
            }
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

        // One MySQL connection per profile per tick — matches the v2 Python
        // shape that avoided host_cache 1129 from N× short-lived connects.
        let mut conn = match connect_source(&cfg_json).await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "self_health: MySQL connect failed for profile {}: {e:#}",
                    profile.entity_folder
                );
                continue;
            }
        };

        // Optional: capture source identity once per profile so we can attach
        // it to every diagnostic without an extra round-trip per table.
        let source_diag = capture_source_diagnostics(&mut conn).await.ok();

        for qkey in &bootstrapped {
            if should_skip(qkey, cfg) {
                debug!("self_health: skip {qkey} (audit/denylist)");
                continue;
            }

            let (schema, table_sql) = parse_qkey(sync_all, &database, qkey);
            let disk_entity = if sync_all { schema.to_lowercase() } else { entity.clone() };
            let mirror_dir = resolve_mirror_dir(data_root, &disk_entity, &table_sql);

            let mirror_count_at = chrono::Utc::now();
            let mirror_count = mirror_live_count(&mirror_dir).unwrap_or(0);

            let source_count_at = chrono::Utc::now();
            let source_count = match mysql_count(&mut conn, sync_all, &database, &schema, &table_sql).await
            {
                Ok(n) => n,
                Err(e) => {
                    warn!("self_health: COUNT({qkey}) failed: {e:#}");
                    continue;
                }
            };

            let diff = source_count as i64 - mirror_count as i64;
            rows.push(TableConsistency {
                table: qkey.clone(),
                source_count,
                mirror_count,
            });

            if diff != 0 && cfg.telemetry_diagnostics_enabled {
                drifts.push(DriftCapture {
                    table: qkey.clone(),
                    diff,
                    source_diag: source_diag.clone(),
                    mirror_dir: mirror_dir.clone(),
                    state_json: state_json.clone(),
                    source_count_at,
                    mirror_count_at,
                });
            }
        }

        // Best-effort: ignore disconnect errors.
        let _ = conn.disconnect().await;
    }

    if rows.is_empty() {
        debug!("self_health: no tables to report this tick.");
        return Ok(());
    }

    let req = ConsistencyCheckRequest {
        checked_at: checked_at.to_rfc3339(),
        tables: rows,
    };
    if let Err(e) = post_consistency_check(
        &identity.control_plane_url,
        &identity.deployment_id,
        &identity.instance_token,
        &req,
    )
    .await
    {
        warn!("self_health: consistency-check POST failed: {e:#}");
        // Don't abort — still try to send diagnostics so root cause isn't lost.
    } else {
        debug!("self_health: reported {} table(s) at {}", req.tables.len(), req.checked_at);
    }

    // Capture + send diagnostics for each drift.
    for drift in drifts {
        let diag = build_diagnostics(&drift);
        if let Err(e) = post_incident_diagnostics(
            &identity.control_plane_url,
            &identity.deployment_id,
            &identity.instance_token,
            &diag,
        )
        .await
        {
            warn!(
                "self_health: incident-diagnostics POST failed for {}: {e:#}",
                drift.table
            );
        } else {
            info!(
                "self_health: drift diagnostic sent for {} (diff={})",
                drift.table, drift.diff
            );
        }
    }

    Ok(())
}

// ── helpers ─────────────────────────────────────────────────────────────────

struct ProfileEntry {
    entity_folder: String,
    config_path: PathBuf,
    state_path: PathBuf,
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

fn parse_qkey(sync_all: bool, database: &str, qkey: &str) -> (String, String) {
    if sync_all {
        if let Some((s, t)) = qkey.split_once('.') {
            return (s.to_string(), t.to_string());
        }
    }
    (database.to_string(), qkey.to_string())
}

fn should_skip(qkey: &str, cfg: &EffectiveEngineConfig) -> bool {
    if BUILT_IN_AUDIT_DENYLIST.contains(&qkey) {
        return true;
    }
    if let Some(deny) = &cfg.watch_denylist {
        if deny.iter().any(|d| d == qkey) {
            return true;
        }
    }
    if let Some(allow) = &cfg.watch_allowlist {
        return !allow.iter().any(|a| a == qkey);
    }
    false
}

fn resolve_mirror_dir(data_root: &Path, entity: &str, table: &str) -> PathBuf {
    let primary = data_root.join("mirror").join(entity);
    let direct = primary.join(table);
    if direct.is_dir() {
        return direct;
    }
    // Case-insensitive fallback (MySQL is by default case-insensitive on table names).
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
    // Legacy layout (pre-mirror/ split). Mirrors the Python helper.
    let legacy = data_root.join(entity).join(table);
    if legacy.is_dir() {
        return legacy;
    }
    direct
}

fn mirror_live_count(table_dir: &Path) -> Option<u64> {
    let manifest = table_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest).ok()?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let segments = val.get("segments")?.as_array()?;
    let mut live: i64 = 0;
    let mut seen_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for seg in segments {
        let rc = seg.get("record_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let dc = seg.get("deleted_count").and_then(|v| v.as_i64()).unwrap_or(0);
        live += (rc - dc).max(0);
        if let Some(id) = seg.get("id").and_then(|v| v.as_u64()) {
            seen_ids.insert(id);
        }
    }
    // Also include the active segment. Its rc/dc are not in manifest.segments
    // (those are immutable only). Derive rc from the primary-key column's
    // .offsets file size (8 bytes per row — same layout for every customer
    // table because the segment writer always writes one u64 offset per row
    // per column), and dc from deleted.bitmap on disk. Table::delete
    // persists deleted.bitmap on every CDC delete event, so the on-disk
    // view trails in-memory by at most one in-flight call. Without this,
    // a freshly-compacted or freshly-rotated table whose only live rows
    // live in active reads as 0 here — and self_health raises a
    // false-positive drift incident against a mirror that actually matches
    // source.
    if let Some(active_id) = val.get("active_segment_id").and_then(|v| v.as_u64()) {
        if !seen_ids.contains(&active_id) {
            // The PK column name is per-table — read it from the manifest so
            // this works for any customer schema (PK might be `id`, `uuid`,
            // `customer_id`, etc.). Fall back to scanning the segment dir for
            // any `*.offsets` file if the manifest has no primary_key set;
            // every column writes one offset per row so any will yield the
            // same record count.
            let pk_field = val
                .get("primary_key")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let active_path = table_dir
                .join("segments")
                .join(format!("seg_{:04}", active_id));
            let offsets_path = match pk_field {
                Some(name) => Some(active_path.join(format!("{}.offsets", name))),
                None => std::fs::read_dir(&active_path).ok().and_then(|entries| {
                    entries.flatten().find_map(|e| {
                        let name = e.file_name();
                        let name = name.to_string_lossy();
                        if name.ends_with(".offsets") {
                            Some(e.path())
                        } else {
                            None
                        }
                    })
                }),
            };
            if let Some(p) = offsets_path {
                if let Ok(meta) = std::fs::metadata(&p) {
                    let active_rc = (meta.len() / 8) as i64;
                    let mut active_dc: i64 = 0;
                    if let Ok(file) = std::fs::File::open(active_path.join("deleted.bitmap")) {
                        if let Ok(bm) = roaring::RoaringBitmap::deserialize_from(file) {
                            active_dc = bm.len() as i64;
                        }
                    }
                    live += (active_rc - active_dc).max(0);
                }
            }
        }
    }
    Some(live as u64)
}

async fn connect_source(cfg_json: &serde_json::Value) -> Result<Conn> {
    let host = cfg_json.get("host").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let port = cfg_json.get("port").and_then(|v| v.as_u64()).unwrap_or(3306) as u16;
    let user = cfg_json.get("user").and_then(|v| v.as_str()).unwrap_or("").to_string();
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
        // RDS terminates TLS. Same heuristic + same trust posture as the Python
        // reporter that ran for months: opportunistic TLS, no cert chain
        // verification (pymysql `ssl={'ssl':{}}` defaults). The threat model
        // for engine ⇄ same-VPC RDS is interception by another instance
        // already inside the VPC — out of scope here. If a future config
        // needs strict verification it goes through engine_configs, not env.
        builder = builder.ssl_opts(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        );
    }

    let opts: Opts = builder.into();
    let conn = Conn::new(opts).await.with_context(|| format!("connect to {host}"))?;
    Ok(conn)
}

async fn mysql_count(
    conn: &mut Conn,
    sync_all: bool,
    database: &str,
    schema: &str,
    table: &str,
) -> Result<u64> {
    let ident = |s: &str| s.replace('`', "``");
    let sql = if sync_all {
        format!("SELECT COUNT(*) FROM `{}`.`{}`", ident(schema), ident(table))
    } else {
        // Set the active database explicitly. Cheaper than a full sql_use_db
        // round-trip on every call but `USE` is fine over mysql_async.
        let use_sql = format!("USE `{}`", ident(database));
        conn.query_drop(use_sql).await?;
        format!("SELECT COUNT(*) FROM `{}`", ident(table))
    };
    let n: u64 = conn
        .query_first(sql)
        .await?
        .ok_or_else(|| anyhow!("COUNT returned no row"))?;
    Ok(n)
}

async fn capture_source_diagnostics(conn: &mut Conn) -> Result<SourceDiagnostics> {
    let version: Option<String> = conn.query_first("SELECT VERSION()").await?;
    let binlog_format: Option<String> = conn
        .query_first("SELECT @@global.binlog_format")
        .await
        .ok()
        .flatten();
    let isolation: Option<String> = conn
        .query_first("SELECT @@global.transaction_isolation")
        .await
        .ok()
        .flatten();
    Ok(SourceDiagnostics {
        mysql_version: version,
        binlog_format,
        isolation,
    })
}

struct DriftCapture {
    table: String,
    diff: i64,
    source_diag: Option<SourceDiagnostics>,
    mirror_dir: PathBuf,
    state_json: serde_json::Value,
    source_count_at: chrono::DateTime<chrono::Utc>,
    mirror_count_at: chrono::DateTime<chrono::Utc>,
}

fn build_diagnostics(d: &DriftCapture) -> DriftDiagnosticsRequest {
    let engine_version = option_env!("CARGO_PKG_VERSION").map(|v| format!("v{v}"));

    let cdc = CdcDiagnostics {
        binlog_file: d
            .state_json
            .get("binlog_file")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        binlog_pos: d.state_json.get("binlog_pos").and_then(|v| v.as_u64()),
        gtid: d
            .state_json
            .get("gtid_executed")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        last_event_at: d
            .state_json
            .get("last_event_unix_ms")
            .and_then(|v| v.as_u64())
            .and_then(|ms| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64))
            .map(|dt| dt.to_rfc3339()),
        worker_state: Some(
            if let Some(ms) = d
                .state_json
                .get("last_mirror_batch_unix_ms")
                .and_then(|v| v.as_u64())
            {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|s| s.as_millis() as u64)
                    .unwrap_or(0);
                let lag = now_ms.saturating_sub(ms) / 1000;
                if lag < 60 {
                    "live".into()
                } else {
                    "lagging".into()
                }
            } else {
                "unknown".into()
            },
        ),
        lag_secs: d
            .state_json
            .get("last_mirror_batch_unix_ms")
            .and_then(|v| v.as_u64())
            .map(|ms| {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|s| s.as_millis() as u64)
                    .unwrap_or(0);
                (now_ms.saturating_sub(ms) / 1000) as i64
            }),
        recent_errors: None,
    };

    let (segment_count, last_write_at) = mirror_health(&d.mirror_dir);
    let mirror = MirrorDiagnostics {
        segment_count,
        last_write_at,
    };

    let timing = TimingDiagnostics {
        source_count_at: Some(d.source_count_at.to_rfc3339()),
        mirror_count_at: Some(d.mirror_count_at.to_rfc3339()),
    };

    DriftDiagnosticsRequest {
        captured_at: chrono::Utc::now().to_rfc3339(),
        engine_version,
        table: d.table.clone(),
        diff: d.diff,
        cdc: Some(cdc),
        source: d.source_diag.clone(),
        mirror: Some(mirror),
        timing: Some(timing),
        auto_repair_attempted: false,
        auto_repair_outcome: None,
        notes: None,
    }
}

fn mirror_health(table_dir: &Path) -> (Option<u32>, Option<String>) {
    let manifest = table_dir.join("manifest.json");
    let Ok(raw) = std::fs::read_to_string(&manifest) else {
        return (None, None);
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return (None, None);
    };
    let count = val
        .get("segments")
        .and_then(|v| v.as_array())
        .map(|a| a.len() as u32);
    let last_write_at = std::fs::metadata(&manifest)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(d.as_millis() as i64))
        .map(|dt| dt.to_rfc3339());
    (count, last_write_at)
}
