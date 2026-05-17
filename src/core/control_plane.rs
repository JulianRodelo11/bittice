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
