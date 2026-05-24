//! Typed HTTP client for `bittice-services`. All requests authenticate with the
//! saved API key (`Authorization: Bearer bk_live_…`) except for `heartbeat`,
//! which uses an instance token issued at deployment-create time.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::credentials;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

fn base_url() -> String { credentials::resolved_control_plane_url() }

// ─── /v1/login ──────────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub user_id: String,
    pub email: String,
    pub name: Option<String>,
    pub plan: String,
}

pub async fn login(api_key: &str) -> Result<LoginResponse> {
    let url = format!("{}/v1/login", base_url());
    let resp = client().post(&url)
        .bearer_auth(api_key)
        .send().await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Login failed ({status}): {body}");
    }
    let parsed: LoginResponse = serde_json::from_str(&body)
        .with_context(|| format!("parse /v1/login response: {body}"))?;
    Ok(parsed)
}

// ─── /v1/deployments (create) ───────────────────────────────────────────────
#[derive(Debug, Serialize)]
pub struct CreateDeploymentRequest {
    pub name: String,
    pub cloud_provider: String,
    pub region: String,
    pub instance_type: String,
    pub source_db_engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_db_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_profile_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vpc_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDeploymentResponse {
    pub deployment_id: String,
    pub instance_token: String,
    pub control_plane_url: String,
}

pub async fn create_deployment(api_key: &str, req: &CreateDeploymentRequest) -> Result<CreateDeploymentResponse> {
    let url = format!("{}/v1/deployments", base_url());
    let resp = client().post(&url)
        .bearer_auth(api_key)
        .json(req)
        .send().await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("create_deployment failed ({status}): {body}");
    }
    serde_json::from_str(&body)
        .with_context(|| format!("parse /v1/deployments response: {body}"))
}

// ─── /v1/heartbeat ──────────────────────────────────────────────────────────
#[derive(Debug, Serialize, Default)]
pub struct HeartbeatRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdc_profiles_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdc_profiles_live: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ec2_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_account_id: Option<String>,
    /// EC2 instance type (e.g. `t3.micro`). Read from IMDS at heartbeat
    /// startup; the control plane snapshots it into `usage_hours.instance_type`
    /// so billing reflects what was actually running each hour even if the
    /// deployment is later resized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
    /// Free-form diagnostics for the control plane's "is this customer
    /// healthy?" view. Today the engine fills in:
    ///   - `binlog_file`, `binlog_pos`     (current CDC position)
    ///   - `bootstrapped_tables`           (count of tables in the mirror)
    ///   - `last_mirror_batch_age_secs`    (how stale the mirror is now)
    /// The Lambda stores the whole blob in `deployments.current_extra`,
    /// so we can grow this dict without DB migrations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Heartbeat uses a different auth than the user-facing endpoints: the engine
/// presents its instance_token (bound to the deployment at create time) and
/// echoes the deployment's public_id so the server can look up the token hash
/// without scanning the table.
pub async fn heartbeat(
    control_plane_url: &str,
    deployment_id: &str,
    instance_token: &str,
    req: &HeartbeatRequest,
) -> Result<()> {
    let url = format!("{}/v1/heartbeat", control_plane_url.trim_end_matches('/'));
    let resp = client().post(&url)
        .bearer_auth(instance_token)
        .header("X-Bittice-Deployment", deployment_id)
        .json(req)
        .send().await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("heartbeat failed ({status}): {body}");
    }
    Ok(())
}

// ─── /v1/config (engine reads its behavior from the control plane) ──────────
//
// The engine carries no behavior flags of its own; on startup (and every 60s)
// it asks the control plane what to do. Per Julian's architecture rule: all
// toggles (self_health, auto_repair, telemetry, cadence, watch lists) live in
// the bittice RDS, flipped via UPDATE — never via env vars on the customer VM.

#[derive(Debug, Deserialize, Clone)]
pub struct EffectiveEngineConfig {
    pub self_health_enabled: bool,
    pub self_health_interval_secs: u64,
    pub auto_repair_enabled: bool,
    pub auto_repair_cap_per_day: u32,
    pub auto_repair_min_consecutive_drifts: u32,
    pub telemetry_diagnostics_enabled: bool,
    #[serde(default)]
    pub watch_allowlist: Option<Vec<String>>,
    #[serde(default)]
    pub watch_denylist: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EngineConfigResponse {
    pub config_version: String,
    pub effective_config: EffectiveEngineConfig,
}

/// Result of a config fetch — either fresh config or a 304 confirming the
/// cached version is still valid.
#[derive(Debug)]
pub enum ConfigFetch {
    Fresh(EngineConfigResponse),
    NotModified,
}

pub async fn fetch_engine_config(
    control_plane_url: &str,
    deployment_id: &str,
    instance_token: &str,
    if_none_match: Option<&str>,
) -> Result<ConfigFetch> {
    let url = format!("{}/v1/config", control_plane_url.trim_end_matches('/'));
    let mut req = client().get(&url)
        .bearer_auth(instance_token)
        .header("X-Bittice-Deployment", deployment_id);
    if let Some(etag) = if_none_match {
        req = req.header("If-None-Match", etag);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status.as_u16() == 304 {
        return Ok(ConfigFetch::NotModified);
    }
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("fetch_engine_config failed ({status}): {body}");
    }
    let parsed: EngineConfigResponse = serde_json::from_str(&body)
        .with_context(|| format!("parse /v1/config response: {body}"))?;
    Ok(ConfigFetch::Fresh(parsed))
}

// ─── /v1/health/consistency-check ───────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TableConsistency {
    pub table: String,
    pub source_count: u64,
    pub mirror_count: u64,
}

#[derive(Debug, Serialize)]
pub struct ConsistencyCheckRequest {
    pub checked_at: String, // ISO-8601 UTC
    pub tables: Vec<TableConsistency>,
}

pub async fn post_consistency_check(
    control_plane_url: &str,
    deployment_id: &str,
    instance_token: &str,
    req: &ConsistencyCheckRequest,
) -> Result<()> {
    let url = format!(
        "{}/v1/health/consistency-check",
        control_plane_url.trim_end_matches('/')
    );
    let resp = client().post(&url)
        .bearer_auth(instance_token)
        .header("X-Bittice-Deployment", deployment_id)
        .json(req)
        .send().await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("post_consistency_check failed ({status}): {body}");
    }
    Ok(())
}

// ─── /v1/health/incident-with-diagnostics ───────────────────────────────────

#[derive(Debug, Serialize, Default)]
pub struct CdcDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binlog_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binlog_pos: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gtid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_state: Option<String>, // "live" | "lagging" | "failed" | "unknown"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lag_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_errors: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct SourceDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mysql_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binlog_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct MirrorDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_write_at: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct TimingDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_count_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror_count_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DriftDiagnosticsRequest {
    pub captured_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    pub table: String,
    pub diff: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdc: Option<CdcDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<MirrorDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<TimingDiagnostics>,
    #[serde(default)]
    pub auto_repair_attempted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_repair_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<serde_json::Value>,
}

pub async fn post_incident_diagnostics(
    control_plane_url: &str,
    deployment_id: &str,
    instance_token: &str,
    req: &DriftDiagnosticsRequest,
) -> Result<()> {
    let url = format!(
        "{}/v1/health/incident-with-diagnostics",
        control_plane_url.trim_end_matches('/')
    );
    let resp = client().post(&url)
        .bearer_auth(instance_token)
        .header("X-Bittice-Deployment", deployment_id)
        .json(req)
        .send().await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("post_incident_diagnostics failed ({status}): {body}");
    }
    Ok(())
}
