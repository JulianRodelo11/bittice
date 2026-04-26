use anyhow::Result;
use cliclack::{intro, outro_cancel, select, input, password, spinner};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use crate::core::cdc::CdcWorker;
use crate::core::vpn::VpnManager;
use crate::core::saved_queries::load_operations;
use tracing::info;
use std::process::{Command, Stdio};
use std::io::{self, BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set after the first local `start_all_servers` spawn so returning to the menu does not start duplicate listeners.
static LOCAL_ENGINE_STARTED: AtomicBool = AtomicBool::new(false);

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
    let path = std::path::Path::new("data").join(&info.entity).join("cdc_config.json");
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

fn list_synced_entities() -> Vec<String> {
    let data_dir = std::path::Path::new("data");
    let mut entities = Vec::new();

    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }

            if entry.path().join("cdc_config.json").exists() {
                entities.push(name);
            }
        }
    }

    entities.sort();
    entities
}

/// Direct children of `data/` treated as data environments (CDC or static-only).
/// Includes any non-hidden subdirectory; does not require `cdc_config.json`.
fn list_data_entity_roots() -> Vec<String> {
    let data_dir = std::path::Path::new("data");
    let mut roots = Vec::new();

    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            roots.push(name);
        }
    }

    roots.sort();
    roots
}

fn has_saved_operations() -> bool {
    load_operations()
        .map(|ops| !ops.is_empty())
        .unwrap_or(false)
}

fn deploy_menu_eligible() -> bool {
    !list_data_entity_roots().is_empty() && has_saved_operations()
}

/// Follow `data/server.log` on stdout (filtered). Caller must kill the child on exit. Unix only.
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

fn run_deploy_info_screen() -> io::Result<()> {
    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[1mDeploy\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mBittice se publica como imagen Docker en cada release (tag v*).\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m1. En GitHub: Releases → descarga bittice-server-<versión>.zip\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m2. En el servidor: Docker + docker compose (el .env ya apunta a la imagen GHCR).\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90m3. Documentación en el repo: deploy/README.md y deploy/SERVER_QUICKSTART.md\x1b[0m");
    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mChoose « Back below, or press Esc / Ctrl+C if your terminal sends it.\x1b[0m");

    let mut back = select("Deploy")
        .item((), "« Back to main menu", "Return without leaving Bittice");
    match back.interact() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
        Err(e) => Err(e),
    }
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
        let mut s = select("What should be synchronized?")
            .item(
                0u8,
                "All user databases on this server",
                "One CDC connection; each schema is stored as data/<database_name>/",
            )
            .item(
                1u8,
                "A single database only",
                "Classic mode: pick the database and an optional Bittice folder name",
            )
            .item(
                SEL_BACK_MAIN,
                "« Back to main menu",
                "Leave this wizard without saving",
            );
        s.interact()
    });
    if sync_mode == SEL_BACK_MAIN {
        return Ok(WizardOutcome::Cancelled);
    }

    let (database, sync_all_databases, entity) = if sync_mode == 0 {
        let profile: String = interact_or_cancel!({
            let mut p = input("Connection profile name (folder under data/ for config)")
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
            let mut s = select("Use internal VPN for database connection?")
                .item(0u8, "Yes", "Choose a VPN provider")
                .item(1u8, "No", "Direct connection")
                .item(
                    SEL_BACK_MAIN,
                    "« Back to main menu",
                    "Leave this wizard without saving",
                );
            s.interact()
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
            let mut s = select("Select VPN provider")
                .item(0u8, "OpenVPN", "Use .ovpn file or content")
                .item(1u8, "My provider is not listed", "Request new integration")
                .item(
                    SEL_BACK_MAIN,
                    "« Back to main menu",
                    "Leave this wizard without saving",
                );
            s.interact()
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
                picker = picker.item(i, file, "Stored in persistent VPN folder");
            }
            picker = picker.item(
                back_idx,
                "« Back to main menu",
                "Leave this wizard without saving",
            );
            let chosen_idx: usize = interact_or_cancel!({ picker.interact() });
            if chosen_idx == back_idx {
                return Ok(WizardOutcome::Cancelled);
            }
            vpn_storage.join(&available_configs[chosen_idx]).to_string_lossy().to_string()
        } else {
            interact_or_cancel!({
                let mut p = input("Provide OpenVPN configuration (Paste .ovpn content OR enter Path)")
                    .placeholder("/Users/.../vpn.ovpn or config text");
                p.interact()
            })
        };

        if input_val.is_empty() {
            return Err(anyhow::anyhow!("VPN configuration cannot be empty."));
        }

        let final_vpn_path: String;

        if input_val.starts_with("http") {
            let s = spinner();
            s.start("Downloading VPN configuration...");
            let response = reqwest::get(&input_val).await?;
            let bytes = response.bytes().await?;
            let file_name = input_val.split('/').last().unwrap_or("downloaded.ovpn");
            let dest_path = vpn_storage.join(file_name);
            std::fs::write(&dest_path, bytes)?;
            final_vpn_path = dest_path.to_string_lossy().to_string();
            s.stop("✓ Download complete.");
        } else if input_val.contains("client") && input_val.contains("dev") {
            let dest_path = vpn_storage.join("pasted_config.ovpn");
            std::fs::write(&dest_path, &input_val)?;
            final_vpn_path = dest_path.to_string_lossy().to_string();
            info!("Using pasted VPN configuration.");
        } else {
            let normalized_input = input_val.replace("\\", "/");
            let parts: Vec<&str> = normalized_input.split('/').filter(|s: &&str| !s.is_empty()).collect();

            let mut found_path = None;
            if std::path::Path::new(&input_val).exists() {
                found_path = Some(input_val.clone());
            } else {
                let is_linux = cfg!(target_os = "linux");
                if is_linux && (input_val.starts_with("/Users/") || input_val.starts_with("C:\\") || input_val.starts_with("/home/")) {
                    return Err(anyhow::anyhow!(
                        "Path Error: You are providing a LOCAL path from your PC ('{}'),\nbut Bittice is running on a REMOTE Cloud Instance.\n\nTips:\n1. Open the .ovpn file on your computer.\n2. COPY all its text content.\n3. PASTE it here directly.",
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
                anyhow::anyhow!("File not found at: {}.\nTips:\n- Make sure the file is inside your PC's Home folder.\n- Or just copy and paste the TEXT of the .ovpn file here.", input_val)
            })?;

            let path = std::path::Path::new(&final_path_to_copy);
            let file_name = path.file_name().ok_or(anyhow::anyhow!("Invalid file name"))?;
            let dest_path = vpn_storage.join(file_name);
            if path != dest_path { std::fs::copy(&path, &dest_path)?; }
            final_vpn_path = dest_path.to_string_lossy().to_string();
        }

        if !VpnManager::is_installed() {
            let install_vpn: u8 = interact_or_cancel!({
                let mut s = select("OpenVPN is not installed. Install it now?")
                    .item(0u8, "Yes", "Try automatic installation (requires sudo)")
                    .item(1u8, "No", "Abort this connection setup")
                    .item(
                        SEL_BACK_MAIN,
                        "« Back to main menu",
                        "Leave this wizard without saving",
                    );
                s.interact()
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
    println!("\x1b[90m│\x1b[0m  \x1b[90mTip: In lists, choose « Back or Exit when you see them. Esc / Ctrl+C also cancel prompts when the terminal forwards those keys.\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mFrom the live monitor, press Ctrl+C to return to this menu (engine keeps running). Esc is not used there.\x1b[0m");

    'session: loop {
    let (option, monitor_scope): (u8, String) = 'main: loop {
        let deploy_ok = deploy_menu_eligible();
        let mut main_sel = select("Select operation mode")
            .item(
                0u8,
                "Connect and synchronize to a database",
                "Configure a new MySQL CDC connection",
            )
            .item(
                1u8,
                "Use Bittice with synchronized databases",
                "Load and monitor all synchronized entities",
            );

        if deploy_ok {
            main_sel = main_sel.item(
                2u8,
                "Deploy",
                "Docker image & server bundle (GitHub Releases)",
            );
        }

        let exit_id: u8 = if deploy_ok { 3 } else { 2 };
        main_sel = main_sel.item(exit_id, "Exit", "Quit Bittice");

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
                        break 'main (
                            0u8,
                            if cdc_info.sync_all_databases {
                                format!(
                                    "connection profile '{}' (all schemas on host)",
                                    cdc_info.entity
                                )
                            } else {
                                format!("all synchronized entities (new: {})", cdc_info.entity)
                            },
                        );
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
                if let Err(e) = run_deploy_info_screen() {
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e.into());
                    }
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
    println!("\x1b[90m│\x1b[0m  \x1b[90mPress Ctrl+C to return to the main menu.\x1b[0m");
    println!("\x1b[90m│\x1b[0m");

    let is_docker_only = std::path::Path::new("/.dockerenv").exists();

    if is_docker_only {
        let client = reqwest::Client::new();
        let _ = client.post("http://localhost:3000/_config/reload")
            .send()
            .await;

        println!("\x1b[90m│\x1b[0m");
        println!("\x1b[32m◆\x1b[0m  \x1b[1mBittice Engine Updated!\x1b[0m");
        println!("\x1b[90m│\x1b[0m  \x1b[90mThe background engine has automatically loaded the new entity.\x1b[0m");
        println!("\x1b[90m│\x1b[0m");
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    } else {
        let first_local_start = !LOCAL_ENGINE_STARTED.swap(true, Ordering::SeqCst);
        if first_local_start {
            tokio::spawn(async move {
                let _ = crate::server::start_all_servers(None, false).await;
            });
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    }

    let log_path = "data/server.log";
    println!("\x1b[90m│\x1b[0m");
    println!(
        "\x1b[90m│\x1b[0m  \x1b[90mSync:\x1b[0m MySQL → local tables runs in the engine (binlog CDC). Engine logs go to \x1b[1m{}\x1b[0m\x1b[90m; filtered lines stream below when available.\x1b[0m",
        log_path
    );
    println!("\x1b[90m│\x1b[0m  \x1b[90mIf nothing new appears, you are usually caught up or there is no MySQL traffic yet.\x1b[0m");
    println!("\x1b[90m│\x1b[0m");

    let mut tail_child = spawn_server_log_tail_follow(log_path);
    if tail_child.is_none() {
        println!(
            "\x1b[90m│\x1b[0m  \x1b[90m(Log follow not started — open \x1b[0m{}\x1b[90m in another terminal, or use Unix/macOS for inline tail.)\x1b[0m",
            log_path
        );
    }

    tokio::signal::ctrl_c().await?;
    if let Some(mut c) = tail_child.take() {
        let _ = c.kill();
    }

    println!("\x1b[90m│\x1b[0m");
    println!("\x1b[32m◆\x1b[0m  \x1b[1mBack to main menu\x1b[0m  \x1b[90m(HTTP/gRPC engine keeps running)\x1b[0m");
    println!("\x1b[90m│\x1b[0m");
    continue 'session;
    }

    #[allow(unreachable_code)]
    Ok(())
}
