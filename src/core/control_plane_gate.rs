//! Gate outbound integration with the Bittice control plane (API key, heartbeat,
//! drift reports). Cloud VM deploy (Terraform + EC2) stays available without this.

/// Heartbeat, self_health, deploy-time API key login, and deployment registration.
/// Off during local-first preview — flip when control-plane services go live.
pub const REPORTING_ENABLED: bool = false;

/// Alias used by heartbeat / self_health spawn gates.
pub const ENABLED: bool = REPORTING_ENABLED;
