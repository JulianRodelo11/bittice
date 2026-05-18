//! Periodic heartbeat to the Bittice control plane.
//!
//! Activated when the engine boots with all three env vars set:
//!   - `BITTICE_DEPLOYMENT_ID`     (dep_<ulid>)
//!   - `BITTICE_INSTANCE_TOKEN`    (tok_<random>)
//!   - `BITTICE_CONTROL_PLANE_URL` (https://api.bittice.dev)
//!
//! Local mode (none of these set) is silent — no network calls, no logs about
//! the control plane. This matches the "local is free / cloud is metered"
//! product rule.
//!
//! Cadence: every 5 minutes after a 10s initial delay (lets CDC reach Phase 4
//! before we start reporting `cdc_profiles_live`).

use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::core::control_plane::{heartbeat, HeartbeatRequest};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);
const INITIAL_DELAY: Duration = Duration::from_secs(10);

/// Spawn the heartbeat loop. Returns immediately. The loop runs until the
/// process exits — never panics, never propagates network errors (it just
/// logs and waits for the next tick).
pub fn spawn_if_configured() {
    let dep_id = match std::env::var("BITTICE_DEPLOYMENT_ID") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            debug!("Heartbeat: BITTICE_DEPLOYMENT_ID not set — local mode, no metering.");
            return;
        }
    };
    let token = match std::env::var("BITTICE_INSTANCE_TOKEN") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            warn!("Heartbeat: BITTICE_DEPLOYMENT_ID set but BITTICE_INSTANCE_TOKEN missing — disabled.");
            return;
        }
    };
    let url = match std::env::var("BITTICE_CONTROL_PLANE_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => {
            warn!("Heartbeat: BITTICE_DEPLOYMENT_ID set but BITTICE_CONTROL_PLANE_URL missing — disabled.");
            return;
        }
    };

    let started_at = Instant::now();
    let image_tag = std::env::var("BITTICE_IMAGE_TAG").ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| option_env!("CARGO_PKG_VERSION").map(|v| format!("v{v}")));

    tokio::spawn(async move {
        tokio::time::sleep(INITIAL_DELAY).await;
        // One-shot IMDS read at task start — instance_type can only change via
        // stop/resize/start, which restarts the container, which re-runs this.
        // Repeating the call on every heartbeat would add network jitter without
        // ever observing a different value mid-run.
        let instance_type = imds_instance_type().await;
        if instance_type.is_none() {
            debug!("Heartbeat: IMDS instance-type read failed — not on EC2 or IMDS disabled.");
        }
        loop {
            let uptime_secs = started_at.elapsed().as_secs();
            let (total, live) = profile_counts();
            let req = HeartbeatRequest {
                image_tag: image_tag.clone(),
                cdc_profiles_total: Some(total),
                cdc_profiles_live: Some(live),
                uptime_secs: Some(uptime_secs),
                public_ip: None,        // server can capture from request peer IP if needed
                ec2_instance_id: None,  // could be filled from IMDS in a future iteration
                aws_account_id: None,
                instance_type: instance_type.clone(),
            };
            match heartbeat(&url, &dep_id, &token, &req).await {
                Ok(()) => debug!("Heartbeat sent to {url} (uptime={uptime_secs}s, live={live}/{total})."),
                Err(e) => warn!("Heartbeat failed: {e}"),
            }
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
        }
    });
}

/// Read the EC2 instance type from the Instance Metadata Service (IMDSv2).
/// Returns `None` on any error (not on EC2, IMDS disabled, network glitch).
/// Never panics, never propagates — heartbeat must keep running either way.
async fn imds_instance_type() -> Option<String> {
    const IMDS_BASE: &str = "http://169.254.169.254";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .ok()?;

    // IMDSv2 requires a session token first (PUT, 6h TTL). Many AMIs disable
    // v1 nowadays so we don't bother with the v1 fallback — if v2 fails the
    // value just stays None and the control plane keeps the previous value
    // via COALESCE.
    let token = client
        .put(format!("{IMDS_BASE}/latest/api/token"))
        .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;

    let value = client
        .get(format!("{IMDS_BASE}/latest/meta-data/instance-type"))
        .header("X-aws-ec2-metadata-token", token)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;

    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Reads `(profiles_total, profiles_live)` from the same data the engine uses.
/// Best-effort: returns `(0, 0)` if we can't determine. The control plane
/// tolerates these being absent or stale by a few minutes.
fn profile_counts() -> (u32, u32) {
    let total = crate::core::data_paths::cdc_profile_count(
        &crate::core::data_paths::resolved_data_root()
    ) as u32;
    // For now we don't track "currently live" granularly — the engine's CDC
    // worker state isn't exposed via a single accessor. Heartbeat reports the
    // configured count and a future change can wire actual live count when CDC
    // exposes a public counter.
    (total, total)
}
