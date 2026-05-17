//! Persistent storage for the API key the user obtained from the Bittice
//! control plane. Local CLI commands (Test, Cdc, MigrateExactIndex, etc.) do
//! NOT require this — only the cloud deploy flow does. Stored at
//! `~/.bittice/credentials.json` with chmod 0600.
//!
//! Format:
//! ```json
//! {
//!   "version": 1,
//!   "control_plane_url": "https://api.bittice.dev",
//!   "api_key": "bk_live_…",
//!   "user_id": "01HXYZ…",
//!   "email": "you@example.com"
//! }
//! ```
//!
//! The control_plane_url is captured at login time so subsequent CLI calls
//! always hit the same backend the user authenticated against.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const FILE_NAME: &str = "credentials.json";
pub const DEFAULT_CONTROL_PLANE_URL: &str = "https://api.bittice.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub version: u8,
    pub control_plane_url: String,
    pub api_key: String,
    pub user_id: String,
    pub email: String,
}

fn credentials_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set; cannot locate credentials file")?;
    Ok(PathBuf::from(home).join(".bittice"))
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(credentials_dir()?.join(FILE_NAME))
}

/// Override-aware control plane URL. Order:
///   1. `BITTICE_CONTROL_PLANE_URL` env var (escape hatch / local dev)
///   2. `control_plane_url` from saved credentials
///   3. `DEFAULT_CONTROL_PLANE_URL` constant
pub fn resolved_control_plane_url() -> String {
    if let Ok(v) = std::env::var("BITTICE_CONTROL_PLANE_URL") {
        let t = v.trim();
        if !t.is_empty() {
            return t.trim_end_matches('/').to_string();
        }
    }
    if let Ok(creds) = load() {
        return creds.control_plane_url.trim_end_matches('/').to_string();
    }
    DEFAULT_CONTROL_PLANE_URL.to_string()
}

pub fn load() -> Result<Credentials> {
    let path = credentials_path()?;
    if !path.is_file() {
        bail!(
            "You are not logged in. Run `bittice login` first.\n\
             (Credentials are stored at {}.)",
            path.display()
        );
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let creds: Credentials = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(creds)
}

pub fn save(creds: &Credentials) -> Result<()> {
    let dir = credentials_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(FILE_NAME);
    let json = serde_json::to_string_pretty(creds)?;
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
