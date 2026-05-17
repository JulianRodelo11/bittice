//! Saved profile hints for the cloud-deploy wizard. **Never stores the API key**
//! — that is prompted on every deploy and lives only in memory during the run.
//!
//! The file at `~/.bittice/credentials.json` (chmod 0600) holds:
//! ```json
//! {
//!   "version": 2,
//!   "control_plane_url": "https://api.bittice.com",
//!   "last_email":   "you@example.com",
//!   "last_user_id": "01HXYZ…"
//! }
//! ```
//!
//! `last_email` is a UX hint ("Welcome back you@example.com — paste your API
//! key:"), not a secret. `control_plane_url` lets self-hosted users keep their
//! endpoint without re-typing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const FILE_NAME: &str = "credentials.json";
pub const DEFAULT_CONTROL_PLANE_URL: &str = "https://api.bittice.com";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileHints {
    #[serde(default = "default_version")]
    pub version: u8,
    #[serde(default = "default_url")]
    pub control_plane_url: String,
    #[serde(default)]
    pub last_email: Option<String>,
    #[serde(default)]
    pub last_user_id: Option<String>,
    // Tolerate old key without complaining — we just ignore it from now on.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub api_key: Option<String>,
}

fn default_version() -> u8 { 2 }
fn default_url() -> String { DEFAULT_CONTROL_PLANE_URL.to_string() }

fn credentials_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set; cannot locate credentials file")?;
    Ok(PathBuf::from(home).join(".bittice"))
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(credentials_dir()?.join(FILE_NAME))
}

/// Override-aware control plane URL. Order:
///   1. `BITTICE_CONTROL_PLANE_URL` env var (escape hatch / local dev)
///   2. `control_plane_url` from saved profile hints
///   3. `DEFAULT_CONTROL_PLANE_URL` constant
pub fn resolved_control_plane_url() -> String {
    if let Ok(v) = std::env::var("BITTICE_CONTROL_PLANE_URL") {
        let t = v.trim();
        if !t.is_empty() {
            return t.trim_end_matches('/').to_string();
        }
    }
    if let Ok(p) = load() {
        return p.control_plane_url.trim_end_matches('/').to_string();
    }
    DEFAULT_CONTROL_PLANE_URL.to_string()
}

/// Read the profile hints file. Returns the default ProfileHints when the file
/// doesn't exist (so callers don't have to special-case first-run).
pub fn load() -> Result<ProfileHints> {
    let path = credentials_path()?;
    if !path.is_file() {
        return Ok(ProfileHints {
            version: default_version(),
            control_plane_url: default_url(),
            last_email: None,
            last_user_id: None,
            api_key: None,
        });
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let hints: ProfileHints = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(hints)
}

pub fn save(hints: &ProfileHints) -> Result<()> {
    let dir = credentials_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(FILE_NAME);
    // Strip api_key on write — defensive: even if somehow set in memory, never persist.
    let safe = ProfileHints { api_key: None, ..hints.clone() };
    let json = serde_json::to_string_pretty(&safe)?;
    fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn clear() -> Result<bool> {
    let path = credentials_path()?;
    if path.is_file() {
        fs::remove_file(&path)
            .with_context(|| format!("remove {}", path.display()))?;
        return Ok(true);
    }
    Ok(false)
}
