use anyhow::Result;
use cliclack::{intro, outro, select, input, password, spinner};
use serde::{Deserialize, Serialize};
use std::thread;
use tokio::sync::mpsc;
use crate::core::cdc::CdcWorker;
use crate::core::vpn::VpnManager;
use tracing::info;
use std::process::{Command, Stdio};
use std::io::{BufRead, BufReader};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct CdcInfo {
    host: String,
    port: String,
    user: String,
    pass: String,
    database: String,
    entity: String,
    vpn_file: Option<String>,
}

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

pub async fn run_startup_cliclack() -> Result<()> {
    intro("Bittice")?;

    let option: u8 = select("Select operation mode")
        .item(0, "Connect and synchronize to a database", "Configure a new MySQL CDC connection")
        .item(1, "Use Bittice with an already connected database", "Enter the monitor for an existing entity")
        .interact()?;

    let selected_entity: String;

    if option == 1 {
        // List available entities
        let data_dir = std::path::Path::new("data");
        let mut entities = Vec::new();
        if let Ok(entries) = std::fs::read_dir(data_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') {
                        entities.push(name);
                    }
                }
            }
        }

        if entities.is_empty() {
            println!("\x1b[90m│\x1b[0m");
            println!("\x1b[33m▲\x1b[0m  \x1b[1mNo entities found\x1b[0m");
            println!("\x1b[90m│\x1b[0m  \x1b[90mYou must connect at least one database first.\x1b[0m");
            return Ok(());
        }

        let mut select_entity = select("Select the Entity you want to use");
        for (i, entity) in entities.iter().enumerate() {
            select_entity = select_entity.item(i, entity, "");
        }
        let entity_idx = select_entity.interact()?;
        selected_entity = entities[entity_idx].clone();
    } else {
        // Option 0: Connection flow - New configuration
        let host: String = input("MySQL Host").default_input("localhost").interact()?;
        let port: String = input("Port").default_input("3306").interact()?;
        let user: String = input("User").default_input("root").interact()?;
        let pass: String = password("Password").mask('*').interact()?;
        let database: String = input("Database to synchronize").placeholder("name").interact()?;
        let entity: String = input("Entity name in Bittice").default_input(&database).interact()?;

        // Detect environment
        let is_docker_container = std::path::Path::new("/.dockerenv").exists();
        let is_cloud_env = is_docker_container;

        // Preguntar por VPN SOLO si estamos en Docker (donde bittice gestiona el túnel)
        // En local, el usuario usa su propia VPN.
        let use_vpn: bool = if is_docker_container {
            select("Use internal VPN for database connection?")
                .item(true, "Yes", "Choose a VPN provider")
                .item(false, "No", "Direct connection")
                .interact()?
        } else {
            false
        };

        let mut vpn_file = None;
        if use_vpn {
            let vpn_provider: u8 = select("Select VPN provider")
                .item(0, "OpenVPN", "Use .ovpn file or content")
                .item(1, "My provider is not listed", "Request new integration")
                .interact()?;

            if vpn_provider == 1 {
                println!("\x1b[90m│\x1b[0m");
                println!("\x1b[33m▲\x1b[0m  \x1b[1mProvider not yet supported\x1b[0m");
                println!("\x1b[90m│\x1b[0m  \x1b[90mCurrently we only support OpenVPN. Please contact support to add your provider.\x1b[0m");
                println!("\x1b[90m│\x1b[0m");
                return Ok(());
            }

            // OpenVPN logic
            let vpn_storage = crate::core::vpn::VpnManager::storage_dir();
            std::fs::create_dir_all(&vpn_storage)?;
            let available_configs = list_available_ovpn_configs(&vpn_storage);

            let input_val = if is_cloud_env {
                if available_configs.is_empty() {
                    println!("\x1b[34m│\x1b[0m");
                    println!("\x1b[33m▲\x1b[0m  \x1b[1mNo uploaded VPN configs found\x1b[0m");
                    println!("\x1b[90m│\x1b[0m  \x1b[90mUpload your .ovpn file to {} and run setup again.\x1b[0m", vpn_storage.display());
                    println!("\x1b[90m│\x1b[0m  \x1b[90mExample: scp -i key.pem my-vpn.ovpn ec2-user@<ip>:{}/\x1b[0m", vpn_storage.display());
                    return Ok(());
                }

                let mut picker = select("Select the uploaded OpenVPN config");
                for (i, file) in available_configs.iter().enumerate() {
                    picker = picker.item(i, file, "Stored in persistent VPN folder");
                }
                let chosen_idx = picker.interact()?;
                vpn_storage.join(&available_configs[chosen_idx]).to_string_lossy().to_string()
            } else {
                input("Provide OpenVPN configuration (Paste .ovpn content OR enter Path)")
                    .placeholder("/Users/.../vpn.ovpn or config text")
                    .interact()?
            };

            if input_val.is_empty() {
                return Err(anyhow::anyhow!("VPN configuration cannot be empty."));
            }

            let final_vpn_path: String;

            // 1. Check if it's a URL
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
                // 2. It's the content of the file
                let dest_path = vpn_storage.join("pasted_config.ovpn");
                std::fs::write(&dest_path, &input_val)?;
                final_vpn_path = dest_path.to_string_lossy().to_string();
                info!("Using pasted VPN configuration.");
            } else {
                // 3. Smart Path Translation (Windows/Mac/Linux)
                let normalized_input = input_val.replace("\\", "/");
                let parts: Vec<&str> = normalized_input.split('/').filter(|s: &&str| !s.is_empty()).collect();
                
                let mut found_path = None;
                if std::path::Path::new(&input_val).exists() {
                    found_path = Some(input_val.clone());
                } else {
                    // Smart detection of "Local Path on Remote Instance" mistake
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
                let install_vpn: bool = select("OpenVPN is not installed. Install it now?")
                    .item(true, "Yes", "Try automatic installation (requires sudo)")
                    .item(false, "No", "Abort")
                    .interact()?;
                
                if install_vpn {
                    VpnManager::install()?;
                } else {
                    return Err(anyhow::anyhow!("OpenVPN is required for this connection."));
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
            entity,
            vpn_file,
        };

        let _ = save_cdc_config(&cdc_info);
        selected_entity = cdc_info.entity.clone();

        // Sync Spinner for new connection
        let s = spinner();
        s.start("Starting CDC sync engine...");

        let url = format!("mysql://{}:{}@{}:{}/{}",
            cdc_info.user, cdc_info.pass,
            cdc_info.host, cdc_info.port,
            cdc_info.database);

        let (log_tx, mut log_rx) = mpsc::channel::<String>(100);
        let worker_url = url.clone();
        let worker_entity = cdc_info.entity.clone();
        let worker_db = cdc_info.database.clone();

        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let worker = CdcWorker::with_log(worker_url, worker_entity, worker_db, Some(log_tx));
            let _ = rt.block_on(worker.run());
        });

        while let Some(msg) = log_rx.recv().await {
            // Mostrar progreso de tablas mientras esperamos el READY final
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
            // ... rest of log handling ...
            if let Some(err) = msg.strip_prefix("CDC_ERROR: ") {
                let err_str = err.to_string();
                s.stop(format!("✗ Error: {}", err_str));
                return Err(anyhow::anyhow!(err_str));
            }
            if let Some(warn) = msg.strip_prefix("WARN: ") {
                s.set_message(format!("\x1b[33m▲\x1b[0m  {}", warn));
                continue;
            }
            // Evitar imprimir líneas que contengan secretos o configuraciones sensibles
            if !msg.contains("-----") && !msg.contains("key") && !msg.contains("pass") {
                s.set_message(msg);
            }
        }
    }

    // Always show the banner filtered for the selected entity
    crate::server::show_banner_with_filter(Some(selected_entity.clone()));
    
    // Check if we are already running in Docker
    let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
    if is_docker && option == 0 {
        println!("\x1b[90m│\x1b[0m");
        println!("\x1b[32m◆\x1b[0m  \x1b[1mDocker Environment\x1b[0m");
        println!("\x1b[90m│\x1b[0m  \x1b[90mConfiguration saved. Your background Bittice engine will load the new entity automatically.\x1b[0m");
    }

    println!("\x1b[90m│\x1b[0m");
    // Integrated Live Monitor filtered by entity
    println!("\x1b[32m◆\x1b[0m  \x1b[1mLive Monitor\x1b[0m");
    println!("\x1b[90m│\x1b[0m  \x1b[90mMonitoring events for '{}' in real-time.\x1b[0m", selected_entity);
    println!("\x1b[90m│\x1b[0m");

    // Flow separation for Setup Completion
    let is_docker = std::path::Path::new("/.dockerenv").exists();

    if is_docker {
        // --- DOCKER FLOW: NOTIFY BACKGROUND ENGINE ---
        let client = reqwest::Client::new();
        let _ = client.post("http://localhost:3000/_config/reload")
            .send()
            .await;
        
        println!("\x1b[90m│\x1b[0m");
        println!("\x1b[32m◆\x1b[0m  \x1b[1mBittice Engine Updated!\x1b[0m");
        println!("\x1b[90m│\x1b[0m  \x1b[90mThe background engine has automatically loaded the new entity.\x1b[0m");
        println!("\x1b[90m│\x1b[0m");
        // Esperamos un momento para que el motor de fondo empiece a loguear la sincronización
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    } else {
        // --- LOCAL FLOW: START SERVER IN-PROCESS ---
        let server_entity = selected_entity.clone();
        tokio::spawn(async move {
            let _ = crate::server::start_all_servers(Some(server_entity)).await;
        });
        // Dar tiempo al servidor local para arrancar antes de mostrar el monitor
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }


    let log_path = "data/server.log";
    if std::path::Path::new(log_path).exists() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("tail -f -n 0 {} | grep --line-buffered -i -E '{}|GET|POST|PUT|DELETE|CDC|Error|Warn|AUTH'", log_path, selected_entity))
            .stdout(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        thread::spawn(move || {
            for line in reader.lines().flatten() {
                println!("{}", line);
            }
        });

        tokio::signal::ctrl_c().await?;
        let _ = child.kill();
    } else {
        tokio::signal::ctrl_c().await?;
    }
    
    outro(format!("Exiting monitor for '{}'. Bittice engine remains active.", selected_entity))?;
    
    Ok(())
}
