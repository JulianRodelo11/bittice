//! Canonical layout under the data root (`data/` by default, or `bittice/data` when present):
//! - `profiles/<entity>/` — CDC connection profiles (`cdc_config.json`, `cdc_state.json`)
//! - `mirror/<entity>/` — replicated table trees (schemas / single-DB entity data)
//! - `vpn/`, `server.log`, `.bittice_ops.json` remain at the data root.
//!
//! Legacy flat layout (`data/<entity>/` mixing profiles and mirrors) is migrated once using `.layout_v2`.
//!
//! **`BITTICE_DATA_ROOT`** (absolute or relative path) overrides automatic root selection.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

pub const ENV_DATA_ROOT: &str = "BITTICE_DATA_ROOT";

fn collect_entities_with_cdc_config(root: &Path) -> HashSet<String> {
    let mut keys = HashSet::new();
    let prof = root.join(PROFILES);
    if prof.is_dir() {
        if let Ok(rd) = fs::read_dir(&prof) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && e.path().join("cdc_config.json").is_file()
                {
                    keys.insert(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if reserved_root_dir(&name) {
                continue;
            }
            if e.path().join("cdc_config.json").is_file() && !keys.contains(&name) {
                keys.insert(name);
            }
        }
    }
    keys
}

pub const DATA_DIR_NAME: &str = "data";
pub const ALT_DATA_PREFIX: &str = "bittice/data";
pub const PROFILES: &str = "profiles";
pub const MIRROR: &str = "mirror";
pub const LAYOUT_MARKER: &str = ".layout_v2";

/// Resolves the storage root used for profiles, mirrors, logging, and CDC state.
///
/// When **both** `./data` and `./bittice/data` exist (common after tooling creates an empty nested tree),
/// we pick whichever subtree actually contains CDC profiles so mirroring does not silently target an empty tree.
pub fn resolved_data_root() -> PathBuf {
    if let Ok(v) = std::env::var(ENV_DATA_ROOT) {
        let t = v.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }

    let alt = PathBuf::from(ALT_DATA_PREFIX);
    let primary = PathBuf::from(DATA_DIR_NAME);
    let alt_ok = alt.is_dir();
    let pri_ok = primary.is_dir();

    if alt_ok && pri_ok {
        let alt_n = collect_entities_with_cdc_config(&alt).len();
        let pri_n = collect_entities_with_cdc_config(&primary).len();

        if alt_n != pri_n && (alt_n + pri_n) > 0 {
            tracing::warn!(
                "Both '{}' ({}) and '{}' ({}) contain CDC configs under different roots — using '{}' (tie favors {}). Set {} to force one tree.",
                ALT_DATA_PREFIX,
                alt_n,
                DATA_DIR_NAME,
                pri_n,
                if pri_n >= alt_n {
                    DATA_DIR_NAME
                } else {
                    ALT_DATA_PREFIX
                },
                DATA_DIR_NAME,
                ENV_DATA_ROOT
            );
        }

        if pri_n >= alt_n {
            primary
        } else {
            alt
        }
    } else if alt_ok {
        alt
    } else {
        primary
    }
}

pub fn profiles_root() -> PathBuf {
    resolved_data_root().join(PROFILES)
}

pub fn mirror_root() -> PathBuf {
    resolved_data_root().join(MIRROR)
}

pub fn profile_dir(entity: &str) -> PathBuf {
    profiles_root().join(entity)
}

/// Directory where indexed tables live for `entity` (MySQL schema name or single-DB entity folder).
pub fn mirror_entity_dir(entity: &str) -> PathBuf {
    let new_path = mirror_root().join(entity);
    if new_path.exists() {
        return new_path;
    }
    let legacy = resolved_data_root().join(entity);
    if legacy.exists()
        && entity != PROFILES
        && entity != MIRROR
        && entity != "vpn"
        && !legacy.join("cdc_config.json").exists()
    {
        return legacy;
    }
    new_path
}

fn has_manifest_child(dir: &Path) -> bool {
    fs::read_dir(dir).map_or(false, |rd| {
        rd.flatten().any(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false) && e.path().join("manifest.json").exists()
        })
    })
}

fn reserved_root_dir(name: &str) -> bool {
    matches!(name, PROFILES | MIRROR | "vpn") || name.starts_with('.')
}

/// One-time migration from pre-v2 flat `data/<name>/` trees.
pub fn migrate_legacy_layout() -> Result<()> {
    let root = resolved_data_root();
    fs::create_dir_all(&root).context("create data root")?;

    let marker = root.join(LAYOUT_MARKER);
    if marker.exists() {
        return Ok(());
    }

    fs::create_dir_all(root.join(PROFILES))?;
    fs::create_dir_all(root.join(MIRROR))?;

    let entries: Vec<_> = fs::read_dir(&root)
        .with_context(|| format!("read_dir {}", root.display()))?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();

    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if reserved_root_dir(&name) {
            continue;
        }

        let path = entry.path();
        let has_cdc = path.join("cdc_config.json").exists();

        let mut table_dirs: Vec<String> = fs::read_dir(&path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|e| e.path().join("manifest.json").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();

        table_dirs.sort();
        table_dirs.dedup();

        let dest_prof = root.join(PROFILES).join(&name);
        let dest_mirror = root.join(MIRROR).join(&name);

        if has_cdc && !table_dirs.is_empty() {
            if !dest_prof.exists() {
                fs::create_dir_all(&dest_prof)?;
                for f in ["cdc_config.json", "cdc_state.json"] {
                    let from = path.join(f);
                    if from.exists() && !dest_prof.join(f).exists() {
                        fs::rename(&from, dest_prof.join(f))?;
                    }
                }
            }
            if !dest_mirror.exists() {
                fs::create_dir_all(&dest_mirror)?;
                for t in &table_dirs {
                    let from = path.join(t);
                    let to = dest_mirror.join(t);
                    if from.exists() && !to.exists() {
                        fs::rename(&from, to)?;
                    }
                }
            }
            let _ = fs::remove_dir_all(&path);
            info!(
                "data_paths: migrated split profile+tables for '{}' into profiles/ and mirror/",
                name
            );
        } else if has_cdc && table_dirs.is_empty() {
            if !dest_prof.exists() {
                fs::rename(&path, &dest_prof)?;
                info!("data_paths: migrated profile '{}' -> profiles/", name);
            }
        } else if !has_cdc && has_manifest_child(&path) {
            if !dest_mirror.exists() {
                fs::rename(&path, &dest_mirror)?;
                info!("data_paths: migrated mirror '{}' -> mirror/", name);
            }
        }
    }

    fs::write(&marker, b"v2\n").context("write layout marker")?;
    Ok(())
}

/// Every `cdc_config.json` path under `data_root` (`profiles/*/`, legacy entity dirs).
pub fn scan_all_cdc_config_paths_in_data_root(data_root: &Path) -> Vec<PathBuf> {
    let root = data_root;
    let mut out = Vec::new();

    let prof = root.join(PROFILES);
    if prof.is_dir() {
        if let Ok(rd) = fs::read_dir(&prof) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let p = e.path().join("cdc_config.json");
                    if p.is_file() {
                        out.push(p);
                    }
                }
            }
        }
    }

    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if reserved_root_dir(&name) {
                continue;
            }
            let p = e.path().join("cdc_config.json");
            if p.is_file() {
                out.push(p);
            }
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Every `cdc_config.json` path (profiles first, then legacy flat discover).
pub fn scan_all_cdc_config_paths() -> Vec<PathBuf> {
    scan_all_cdc_config_paths_in_data_root(&resolved_data_root())
}

pub fn server_log_path() -> PathBuf {
    resolved_data_root().join("server.log")
}

pub fn bittice_ops_path() -> PathBuf {
    resolved_data_root().join(".bittice_ops.json")
}

/// Number of CDC profiles under `data_root` (for post-deploy health checks).
pub fn cdc_profile_count(data_root: &Path) -> usize {
    scan_all_cdc_config_paths_in_data_root(data_root).len()
}

/// Entity roots that contain table data (mirror + legacy flat mirror dirs).
pub fn iter_mirror_entity_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = resolved_data_root();
    let mirror_r = root.join(MIRROR);
    if mirror_r.is_dir() {
        if let Ok(rd) = fs::read_dir(&mirror_r) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    out.push(e.path());
                }
            }
        }
    }
    if let Ok(rd) = fs::read_dir(&root) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if reserved_root_dir(&name) {
                continue;
            }
            let p = e.path();
            if p.join("cdc_config.json").exists() {
                continue;
            }
            if has_manifest_child(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
