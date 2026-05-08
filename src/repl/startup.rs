use anyhow::Result;
use cliclack::{intro, outro_cancel, select, input, password, spinner};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use crate::core::cdc::CdcWorker;
use crate::core::vpn::VpnManager;
use tracing::{info, warn};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::deploy_pipeline::{self, BuildPlatform, FullDeployConfig};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct CdcInfo {
    host: String,
    port: String,
    user: String,
    pass: String,
    #[serde(default)]
    database: String,
    /// When true: sync every non-system database; data paths use real MySQL schema names.
    #[serde(default)]
    sync_all_databases: bool,
    entity: String,
    vpn_file: Option<String>,
}

enum WizardOutcome {
    Cancelled,
    Done(CdcInfo),
}

/// Reserved `select` value: explicit return to the main menu (works when Esc does not).
const SEL_BACK_MAIN: u8 = 240;

fn save_cdc_config(info: &CdcInfo) -> anyhow::Result<()> {
    let path = crate::core::data_paths::profile_dir(&info.entity).join("cdc_config.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(info)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn list_available_ovpn_configs(vpn_storage: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(vpn_storage) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".ovpn") {
                    files.push(name);
                }
            }
        }
    }
    files.sort();
    files
}

/// Persists a user's .ovpn (path, http(s) URL, or pasted content) into `vpn_storage`.
/// Returns the path string of the stored file (for `cdc_config` or logs).
async fn copy_ovpn_input_to_vpn_storage(
    input_val: &str,
    vpn_storage: &std::path::Path,
) -> Result<String> {
    if input_val.is_empty() {
        return Err(anyhow::anyhow!("VPN configuration cannot be empty."));
    }

    if input_val.starts_with("http://") || input_val.starts_with("https://") {
        let s = spinner();
        s.start("Downloading VPN configuration...");
        let response = reqwest::get(input_val).await?;
        let bytes = response.bytes().await?;
        let file_name = input_val.split('/').last().unwrap_or("downloaded.ovpn");
        let dest_path = vpn_storage.join(file_name);
        std::fs::write(&dest_path, bytes)?;
        let final_vpn_path = dest_path.to_string_lossy().to_string();
        s.stop("✓ Download complete.");
        return Ok(final_vpn_path);
    }

    if input_val.contains("client") && input_val.contains("dev") {
        let dest_path = vpn_storage.join("pasted_config.ovpn");
        std::fs::write(&dest_path, input_val)?;
        info!("Using pasted VPN configuration.");
        return Ok(dest_path.to_string_lossy().to_string());
    }

    let normalized_input = input_val.replace("\\", "/");
    let parts: Vec<&str> = normalized_input
        .split('/')
        .filter(|s: &&str| !s.is_empty())
        .collect();

    let mut found_path = None;
    if std::path::Path::new(input_val).exists() {
        found_path = Some(input_val.to_string());
    } else {
        let is_linux = cfg!(target_os = "linux");
        if is_linux
            && (input_val.starts_with("/Users/")
                || input_val.starts_with("C:\\")
                || input_val.starts_with("/home/"))
        {
            return Err(anyhow::anyhow!(
                "Path Error: You are using a local machine path ('{}') on a cloud Linux instance.\n\n1. Open the .ovpn on your computer.\n2. Copy the full text.\n3. Paste it here.",
                input_val
            ));
        }

        for i in 0..parts.len() {
            let sub_path = parts[i..].join("/");
            let candidate = format!("/app/host_home/{}", sub_path);
            if std::path::Path::new(&candidate).exists() {
                found_path = Some(candidate);
                break;
            }
        }
    }

    let final_path_to_copy = found_path.ok_or_else(|| {
        anyhow::anyhow!(
            "File not found at: {}.\n- Ensure the path is valid, or paste the .ovpn text (must include client and dev).",
            input_val
        )
    })?;

    let path = std::path::Path::new(&final_path_to_copy);
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid file name"))?;
    let dest_path = vpn_storage.join(file_name);
    if path != dest_path {
        std::fs::copy(path, &dest_path)
            .map_err(|e| anyhow::anyhow!("Failed to copy VPN file: {}", e))?;
    }
    Ok(dest_path.to_string_lossy().to_string())
}

/// Browse folders (cliclack `select`) or type path / URL / pasted `.ovpn` text.
async fn read_ovpn_source_interactive() -> io::Result<String> {
    // Hints stay empty: long (hint) text breaks line-wrap alignment with the frame bar in cliclack.
    println!("\x1b[90m│\x1b[0m  \x1b[90mBrowse: type to filter; pick a file or “text input” at the bottom.\x1b[0m");
    let how: u8 = match select("How do you want to add the OpenVPN file?")
        .item(0u8, "Browse folders and pick an .ovpn", "")
        .item(1u8, "Type path, URL, or paste .ovpn text", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) => return Err(e),
    };

    if how == 0u8 {
        match super::ovpn_picker::browse_for_ovpn_path() {
            Ok(Some(s)) => return Ok(s),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }

    match input("OpenVPN: path, http(s) URL, or full .ovpn text")
        .placeholder("/path/file.ovpn or https://... ")
        .interact()
    {
        Ok(s) => Ok(s),
        Err(e) => Err(e),
    }
}

/// Add an .ovpn for later Docker / export-server-bundle; does not start OpenVPN.
async fn add_ovpn_profile_to_storage_for_deploy() -> Result<()> {
    let vpn_storage = VpnManager::storage_dir();
    std::fs::create_dir_all(&vpn_storage)?;
    println!("\x1b[90m│\x1b[0m  \x1b[90mOnly needed when a sync profile has OpenVPN (`vpn_file` in cdc_config); skip if RDS/MySQL is reachable directly from EC2.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mStores under {} — shipped as ./vpn on EC2 and under data/vpn via rsync.\x1b[0m", vpn_storage.display());
    let input_val: String = match read_ovpn_source_interactive().await {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let saved = copy_ovpn_input_to_vpn_storage(&input_val, &vpn_storage).await?;
    println!("\x1b[32m◆\x1b[0m  Saved OpenVPN profile: \x1b[1m{}\x1b[0m", saved);
    Ok(())
}

fn print_deploy_instructions() {
    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[1mDeploy\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mBittice is published as a Docker image on every v* release.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m1. GitHub → Releases: download bittice-server-<version>.zip\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m2. On the server: Docker + docker compose (the .env already points at the GHCR image).\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m3. Manual bundle: deploy/scripts/export-server-bundle.sh (see deploy/README.md)\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m4. Or: Deploy menu → full SSH deploy or “Update engine image on SSH only”.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m5. Docs: deploy/README.md and deploy/SERVER_QUICKSTART.md\x1b[0m");
    println!("\x1b[90m│\x1b[0m");
}

/// Shared prompts for SSH-based deploy flows (full or image refresh).
async fn prompt_ssh_deploy_config() -> Result<Option<FullDeployConfig>> {
    let default_root = deploy_pipeline::find_bittice_project_root()
        .or_else(|| std::env::current_dir().ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    let root_s: String = match input("Bittice project root (repo with deploy/)")
        .default_input(&default_root)
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let project_root = PathBuf::from(root_s.trim());
    if !project_root.join("deploy/Dockerfile.from-source").is_file() {
        return Err(anyhow::anyhow!(
            "Not the Bittice repo root: missing deploy/Dockerfile.from-source in {}",
            project_root.display()
        ));
    }
    let local_image: String = match input("Docker image name:tag (same on your machine and the server)")
        .default_input("bittice:local-run")
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if local_image.is_empty() {
        return Err(anyhow::anyhow!("Image name is required."));
    }
    let ssh_target: String = match input("SSH target (e.g. ubuntu@ec2-…amazonaws.com)")
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if ssh_target.is_empty() {
        return Err(anyhow::anyhow!("SSH target is required (user@host)."));
    }
    let remote_subdir: String = match input("Remote folder (under the server home) for compose and data")
        .default_input("bittice-run")
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if remote_subdir.is_empty() {
        return Err(anyhow::anyhow!("Remote folder name is required."));
    }
    let pl: u8 = match select("Target server CPU (image must match)")
        .item(0u8, "linux/amd64 (typical x86 cloud VMs)", "")
        .item(1u8, "linux/arm64 (e.g. AWS Graviton)", "")
        .item(2u8, "Native (this computer — no buildx)", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let platform = match pl {
        0 => BuildPlatform::LinuxAmd64,
        1 => BuildPlatform::LinuxArm64,
        _ => BuildPlatform::HostNative,
    };
    Ok(Some(FullDeployConfig {
        project_root,
        local_image,
        ssh_target,
        remote_subdir,
        platform,
    }))
}

async fn run_interactive_full_ssh_deploy() -> Result<()> {
    println!("\x1b[90m│\x1b[0m  \x1b[90mVPN is optional: only profiles with `vpn_file` need .ovpn files in data/vpn/ before deploy (OpenVPN only).\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mBuilds the image in Docker, pushes it over SSH, uploads compose + vpn/, rsyncs data/ twice for large mirrors, then compose up. Requires: Docker, rsync, ssh, keys, python3.\x1b[0m");
    let Some(cfg) = prompt_ssh_deploy_config().await? else {
        return Ok(());
    };
    let res = tokio::task::spawn_blocking(move || deploy_pipeline::run_full_deploy(&cfg))
        .await
        .map_err(|e| anyhow::anyhow!("join error: {e}"))?;
    res?;
    Ok(())
}

async fn run_interactive_ssh_image_refresh() -> Result<()> {
    println!("\x1b[90m│\x1b[0m  \x1b[90mFor fixes/features after the first deploy: rebuild image, docker load on EC2, recreate container — keeps server data.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mSingle-node caveat: ~seconds offline during recreate; large uploads happen while the old container still runs.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mNeeds docker + ssh only unless you rsync data (then rsync on remote too).\x1b[0m");
    let Some(cfg) = prompt_ssh_deploy_config().await? else {
        return Ok(());
    };
    let sync_data: u8 = match select("Merge local data/ onto the server before recreate?")
        .item(0u8, "No — ship new binary/engine only", "")
        .item(1u8, "Yes — queries, vpn, or mirrors changed on this laptop", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let sync_flag = sync_data == 1u8;
    let res =
        tokio::task::spawn_blocking(move || deploy_pipeline::run_ssh_engine_image_refresh(&cfg, sync_flag))
            .await
            .map_err(|e| anyhow::anyhow!("join error: {e}"))?;
    res?;
    Ok(())
}

async fn run_deploy_flow() -> Result<()> {
    println!("\x1b[90m│\x1b[0m  \x1b[90mDirect DB access: no VPN setup required. OpenVPN `.ovpn` only when `vpn_file` is set on a profile.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m(1) Optional OpenVPN; (2) Full SSH deploy; (3) Quick image refresh on EC2 (keep data); (4) Instructions.\x1b[0m");
    let first: u8 = match select("Deploy to a server (Docker)")
        .item(0u8, "Add OpenVPN profile (only if sync uses VPN — OpenVPN → data/vpn/)", "")
        .item(1u8, "Build image + bundle + deploy over SSH (full)", "")
        .item(2u8, "Update engine image on SSH only (reuse server data; optional rsync)", "")
        .item(3u8, "Show deployment instructions only", "")
        .item(SEL_BACK_MAIN, "« Back to main menu", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if first == SEL_BACK_MAIN {
        return Ok(());
    }
    if first == 0u8 {
        add_ovpn_profile_to_storage_for_deploy().await?;
    } else if first == 1u8 {
        run_interactive_full_ssh_deploy().await?;
    } else if first == 2u8 {
        run_interactive_ssh_image_refresh().await?;
    } else if first == 3u8 {
        print_deploy_instructions();
    }
    let _ = match select("Deploy")
        .item((), "« Back to main menu", "")
        .interact()
    {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    Ok(())
}

fn list_synced_entities() -> Vec<String> {
    let mut entities: Vec<String> = crate::core::data_paths::scan_all_cdc_config_paths()
        .into_iter()
        .filter_map(|p| {
            p.parent()
                .and_then(|pa| pa.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
    entities.sort();
    entities.dedup();
    entities
}

/// Deploy / “use synced DBs” only after at least one CDC entity exists (`cdc_config.json` under data).
fn synced_workspace_ready() -> bool {
    !list_synced_entities().is_empty()
}

/// Follow engine log file on stdout (filtered). Caller must kill the child on exit. Unix only.
#[cfg(unix)]
fn spawn_server_log_tail_follow(log_path: &str) -> Option<std::process::Child> {
    if !std::path::Path::new(log_path).exists() {
        return None;
    }
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "tail -f -n 0 {} | grep --line-buffered -i -E 'GET|POST|PUT|DELETE|CDC|CDC_ERROR|Error|Warn|AUTH|binlog|Server started'",
            log_path
        ))
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().flatten() {
            println!("{}", line);
        }
    });
    Some(child)
}

#[cfg(not(unix))]
fn spawn_server_log_tail_follow(_log_path: &str) -> Option<std::process::Child> {
    None
}

macro_rules! interact_or_cancel {
    ($bl:block) => {
        match $bl {
            Ok(x) => x,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                return Ok(WizardOutcome::Cancelled);
            }
            Err(e) => return Err(e.into()),
        }
    };
}

async fn run_connect_wizard() -> Result<WizardOutcome> {
    let host: String = interact_or_cancel!({
        let mut p = input("MySQL Host").default_input("localhost");
        p.interact()
    });
    let port: String = interact_or_cancel!({
        let mut p = input("Port").default_input("3306");
        p.interact()
    });
    let user: String = interact_or_cancel!({
        let mut p = input("User").default_input("root");
        p.interact()
    });
    let pass: String = interact_or_cancel!({
        let mut p = password("Password").mask('*');
        p.interact()
    });

    let sync_mode: u8 = interact_or_cancel!({
        println!("\x1b[90m│\x1b[0m  \x1b[90m“All databases”: one CDC; each stored schema\x1b[0m");
        select("What should be synchronized?")
            .item(0u8, "All user databases on this host", "")
            .item(1u8, "A single database only", "")
            .item(SEL_BACK_MAIN, "« Back to main menu", "")
            .interact()
    });
    if sync_mode == SEL_BACK_MAIN {
        return Ok(WizardOutcome::Cancelled);
    }

    let (database, sync_all_databases, entity) = if sync_mode == 0 {
        let profile: String = interact_or_cancel!({
            let mut p = input("Connection profile name (folder under data/profiles/)")
                .default_input("_bittice_host");
            p.interact()
        });
        println!("\x1b[90m│\x1b[0m  \x1b[90mUse a name that does not match a real MySQL database (e.g. _bittice_host).\x1b[0m");
        (String::new(), true, profile)
    } else {
        let database: String = interact_or_cancel!({
            let mut p = input("Database to synchronize").placeholder("name");
            p.interact()
        });
        let entity: String = interact_or_cancel!({
            let mut p = input("Entity name in Bittice").default_input(&database);
            p.interact()
        });
        (database, false, entity)
    };

    let is_docker_container = std::path::Path::new("/.dockerenv").exists();
    let is_cloud_env = is_docker_container;

    let use_vpn: bool = if is_docker_container {
        let v: u8 = interact_or_cancel!({
            select("Use internal VPN for database connection?")
                .item(0u8, "Yes", "")
                .item(1u8, "No", "")
                .item(SEL_BACK_MAIN, "« Back to main menu", "")
                .interact()
        });
        match v {
            SEL_BACK_MAIN => return Ok(WizardOutcome::Cancelled),
            0 => true,
            _ => false,
        }
    } else {
        false
    };

    let mut vpn_file = None;
    if use_vpn {
        let vpn_provider: u8 = interact_or_cancel!({
            select("Select VPN provider")
                .item(0u8, "OpenVPN", "")
                .item(1u8, "My provider is not listed", "")
                .item(SEL_BACK_MAIN, "« Back to main menu", "")
                .interact()
        });

        if vpn_provider == SEL_BACK_MAIN {
            return Ok(WizardOutcome::Cancelled);
        }

        if vpn_provider == 1 {
            println!("\x1b[90m│\x1b[0m");
            println!("\x1b[33m▲\x1b[0m  \x1b[1mProvider not yet supported\x1b[0m");
            println!("\x1b[90m│\x1b[0m  \x1b[90mCurrently we only support OpenVPN. Please contact support to add your provider.\x1b[0m");
            println!("\x1b[90m│\x1b[0m");
            return Ok(WizardOutcome::Cancelled);
        }

        let vpn_storage = crate::core::vpn::VpnManager::storage_dir();
        std::fs::create_dir_all(&vpn_storage)?;
        let available_configs = list_available_ovpn_configs(&vpn_storage);

        let input_val = if is_cloud_env {
            if available_configs.is_empty() {
                println!("\x1b[34m│\x1b[0m");
                println!("\x1b[33m▲\x1b[0m  \x1b[1mNo uploaded VPN configs found\x1b[0m");
                println!("\x1b[90m│\x1b[0m  \x1b[90mUpload your .ovpn file to {} and run setup again.\x1b[0m", vpn_storage.display());
                println!("\x1b[90m│\x1b[0m  \x1b[90mExample: scp -i key.pem my-vpn.ovpn ec2-user@<ip>:{}/\x1b[0m", vpn_storage.display());
                return Ok(WizardOutcome::Cancelled);
            }

            let back_idx = available_configs.len();
            let mut picker = select("Select the uploaded OpenVPN config");
            for (i, file) in available_configs.iter().enumerate() {
                let label = if file.chars().count() > 48 {
                    format!("{}…", file.chars().take(47).collect::<String>())
                } else {
                    file.clone()
                };
                picker = picker.item(i, label, "");
            }
            picker = picker.item(back_idx, "« Back to main menu", "");
            let chosen_idx: usize = interact_or_cancel!({ picker.interact() });
            if chosen_idx == back_idx {
                return Ok(WizardOutcome::Cancelled);
            }
            vpn_storage.join(&available_configs[chosen_idx]).to_string_lossy().to_string()
        } else {
            match read_ovpn_source_interactive().await {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    return Ok(WizardOutcome::Cancelled);
                }
                Err(e) => return Err(e.into()),
            }
        };

        let final_vpn_path: String = copy_ovpn_input_to_vpn_storage(&input_val, &vpn_storage).await?;

        if !VpnManager::is_installed() {
            let install_vpn: u8 = interact_or_cancel!({
                select("OpenVPN is not installed. Install it now?")
                    .item(0u8, "Yes (needs sudo for apt)", "")
                    .item(1u8, "No", "")
                    .item(SEL_BACK_MAIN, "« Back to main menu", "")
                    .interact()
            });

            match install_vpn {
                SEL_BACK_MAIN => return Ok(WizardOutcome::Cancelled),
                0 => VpnManager::install()?,
                _ => return Err(anyhow::anyhow!("OpenVPN is required for this connection.")),
            }
        }

        let prepared_path = VpnManager::prepare_ovpn_file(&final_vpn_path, &host)?;
        VpnManager::start(&prepared_path)?;
        vpn_file = Some(final_vpn_path);
    }

    let cdc_info = CdcInfo {
        host,
        port,
        user,
        pass,
        database,
        sync_all_databases,
        entity,
        vpn_file,
    };

    let _ = save_cdc_config(&cdc_info);
    Ok(WizardOutcome::Done(cdc_info))
}

async fn run_cdc_initial_sync(cdc_info: &CdcInfo) -> Result<()> {
    let s = spinner();
    s.start("Starting CDC sync engine...");

    let url = if cdc_info.sync_all_databases {
        format!(
            "mysql://{}:{}@{}:{}/",
            cdc_info.user, cdc_info.pass, cdc_info.host, cdc_info.port
        )
    } else {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            cdc_info.user,
            cdc_info.pass,
            cdc_info.host,
            cdc_info.port,
            cdc_info.database
        )
    };

    let (log_tx, mut log_rx) = mpsc::channel::<String>(100);
    let worker_url = url.clone();
    let worker_entity = cdc_info.entity.clone();
    let worker_db = cdc_info.database.clone();
    let worker_sync_all = cdc_info.sync_all_databases;

    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let worker = CdcWorker::with_manager_and_log(
            worker_url,
            worker_entity,
            worker_db,
            Arc::new(crate::server::table_manager::TableManager::new()),
            Some(log_tx),
            worker_sync_all,
            true,
            None,
        );
        let _ = rt.block_on(worker.run());
    });

    while let Some(msg) = log_rx.recv().await {
        if msg.contains("Syncing table") || msg.contains("Table sync") || msg.contains("rows") {
            s.set_message(format!("\x1b[34m→\x1b[0m  {}", msg));
            continue;
        }

        if msg == "CDC_READY" {
            s.stop("✓ Sync established (Real-time enabled).");
            break;
        }
        if msg == "CDC_DISABLED" || msg.contains("Connection timed out") || msg.contains("Access denied") {
            let reason = if msg == "CDC_DISABLED" { "CDC is not enabled on server" } else { "Could not connect to Binlog" };
            s.stop(format!("\x1b[32m◆\x1b[0m  Static data sync established ({}. Real-time updates inactive).", reason));
            break;
        }
        if let Some(err) = msg.strip_prefix("CDC_ERROR: ") {
            let err_str = err.to_string();
            s.stop(format!("✗ Error: {}", err_str));
            return Err(anyhow::anyhow!(err_str));
        }
        if let Some(warn) = msg.strip_prefix("WARN: ") {
            s.set_message(format!("\x1b[33m▲\x1b[0m  {}", warn));
            continue;
        }
        if !msg.contains("-----") && !msg.contains("key") && !msg.contains("pass") {
            s.set_message(msg);
        }
    }

    Ok(())
}

pub async fn run_startup_cliclack() -> Result<()> {
    intro("Bittice")?;

    'session: loop {
    let (option, monitor_scope): (u8, String) = 'main: loop {
        let synced_ok = synced_workspace_ready();
        let mut main_sel =
            select("Select operation mode").item(0u8, "Connect and sync to a database", "");

        if synced_ok {
            main_sel = main_sel
                .item(1u8, "Use Bittice with synced databases", "")
                .item(2u8, "Deploy (Docker / server bundle)", "");
        }

        let exit_id: u8 = if synced_ok { 3 } else { 1 };
        main_sel = main_sel.item(exit_id, "Exit", "");

        let choice = match main_sel.interact() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                outro_cancel("Goodbye.")?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        if choice == exit_id {
            outro_cancel("Goodbye.")?;
            return Ok(());
        }

        match choice {
            0u8 => {
                match run_connect_wizard().await? {
                    WizardOutcome::Cancelled => continue 'main,
                    WizardOutcome::Done(cdc_info) => {
                        run_cdc_initial_sync(&cdc_info).await?;
                        println!("\n\x1b[32m◆\x1b[0m  Initial sync complete. Returning to main menu.");
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                        continue 'main;
                    }
                }
            }
            1u8 => {
                let entities = list_synced_entities();

                if entities.is_empty() {
                    println!("\x1b[90m│\x1b[0m");
                    println!("\x1b[33m▲\x1b[0m  \x1b[1mNo synchronized entities found\x1b[0m");
                    println!("\x1b[90m│\x1b[0m  \x1b[90mYou must connect and synchronize at least one database first.\x1b[0m");
                    continue 'main;
                }

                break 'main (
                    1u8,
                    format!("all synchronized entities ({})", entities.join(", ")),
                );
            }
            2u8 => {
                if !synced_ok {
                    continue 'main;
                }
                if let Err(e) = run_deploy_flow().await {
                    return Err(e);
                }
                continue 'main;
            }
            _ => continue 'main,
        }
    };

    crate::server::show_banner();

    let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
    if is_docker && option == 0 {
        println!("\x1b[90m│\x1b[0m");
        println!("\x1b[32m◆\x1b[0m  \x1b[1mDocker Environment\x1b[0m");
        println!("\x1b[90m│\x1b[0m  \x1b[90mConfiguration saved. Your background Bittice engine will load the new entity automatically.\x1b[0m");
    }

    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[1mLive Monitor\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mMonitoring events for {} in real-time.\x1b[0m", monitor_scope);
    println!("\x1b[90m│\x1b[0m");

    let log_path_s = crate::core::data_paths::server_log_path()
        .to_string_lossy()
        .into_owned();
    // Start tail before the local engine so lines like `CDC: entity='…'` are not missed (`tail -n 0`
    // only shows bytes appended after tail opens).
    let mut tail_child = spawn_server_log_tail_follow(&log_path_s);

    let is_docker_only = std::path::Path::new("/.dockerenv").exists();

    if is_docker_only {
        let client = reqwest::Client::new();
        let _ = client.post(crate::server::http_config_reload_url())
            .send()
            .await;

        println!("\x1b[90m│\x1b[0m");
        println!("\x1b[32m◆\x1b[0m  \x1b[1mBittice Engine Updated!\x1b[0m");
        println!("\x1b[90m│\x1b[0m  \x1b[90mThe background engine has automatically loaded the new entity.\x1b[0m");
        println!("\x1b[90m│\x1b[0m");
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    } else {
        if let Err(e) =
            tokio::task::spawn_blocking(|| crate::server::repl_stop_engine_and_join_cdc()).await
        {
            warn!("Pre-start engine shutdown task failed: {}", e);
        }
        tokio::spawn(async move {
            let _ = crate::server::start_all_servers(None, false).await;
        });
    }

    tokio::signal::ctrl_c().await?;

    if let Some(mut c) = tail_child.take() {
        let _ = c.kill();
    }

    // One-line spinner; cleared when shutdown completes (no extra │ / ◆ banners).
    println!();
    const SPIN_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];

    if is_docker_only {
        let mut interval = tokio::time::interval(Duration::from_millis(90));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(360) {
            interval.tick().await;
            let i = (start.elapsed().as_millis() / 90) as usize;
            print!(
                "\r\x1b[2K\x1b[35m{}\x1b[0m  Leaving Live Monitor…",
                SPIN_FRAMES[i % 4]
            );
            let _ = io::stdout().flush();
        }
    } else {
        match tokio::time::timeout(
            Duration::from_secs(120),
            async {
                let mut shutdown = tokio::task::spawn_blocking(|| {
                    crate::server::repl_stop_engine_and_join_cdc()
                });
                let mut interval = tokio::time::interval(Duration::from_millis(90));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut frame: usize = 0;
    loop {
                    tokio::select! {
                        res = &mut shutdown => {
                            res.map_err(|e| anyhow::anyhow!(e))?;
                            return Ok::<(), anyhow::Error>(());
                        }
                        _ = interval.tick() => {
                            print!(
                                "\r\x1b[2K\x1b[35m{}\x1b[0m  Leaving Live Monitor…",
                                SPIN_FRAMES[frame % 4]
                            );
                            let _ = io::stdout().flush();
                            frame += 1;
                        }
                    }
                }
            },
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Engine shutdown: {}", e),
            Err(_) => {
                warn!(
                    "Engine shutdown still running after 120s (CDC may be finishing a long query). \
Returning to menu; stop orphaned workers with: kill bittice or restart the terminal session."
                );
            }
        }
    }

    print!("\r\x1b[2K");
    let _ = io::stdout().flush();
    println!();
    continue 'session;
    }

    #[allow(unreachable_code)]
    Ok(())
}
