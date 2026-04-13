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

        // Detectar si estamos en un entorno que podría necesitar VPN o Docker
        let is_docker_container = std::path::Path::new("/.dockerenv").exists();
        let is_cloud_env = std::env::var("BITTICE_HOST").is_ok() || 
                           std::env::var("BITTICE_ENTITY").is_ok() ||
                           is_docker_container;

        // Preguntar por VPN si estamos en Cloud o si el usuario lo solicita
        let use_vpn: bool = if is_cloud_env {
            select("Use VPN for database connection?")
                .item(true, "Yes", "Choose a VPN provider")
                .item(false, "No", "Direct connection")
                .interact()?
        } else {
            select("Database connection type")
                .item(false, "Direct Connection", "Local or reachable network")
                .item(true, "VPN Connection", "OpenVPN tunnel required")
                .interact()?
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
            let input_val = if is_cloud_env {
                println!("\x1b[34m│\x1b[0m");
                println!("\x1b[32m◆\x1b[0m  \x1b[1mProvide OpenVPN configuration\x1b[0m");
                println!("\x1b[90m│\x1b[0m  \x1b[90mPaste your .ovpn content below.\x1b[0m");
                println!("\x1b[90m│\x1b[0m  \x1b[90m(Your input will be hidden for privacy. Type 'END' + Enter when finished)\x1b[0m");
                println!("\x1b[90m│\x1b[0m");

                let mut buffer = String::new();
                
                // Deshabilitar el eco de la terminal para privacidad
                let term = console::Term::stdout();
                
                loop {
                    let line = term.read_line()?;
                    if line.trim() == "END" { break; }
                    if line.is_empty() { break; }
                    buffer.push_str(&line);
                    buffer.push('\n');
                    
                    // Auto-detección del final del archivo
                    if line.contains("-----END OpenVPN Static key V1-----") || 
                       line.contains("</ca>") || 
                       line.contains("</tls-auth>") {
                        break;
                    }
                }
                println!("\x1b[90m│\x1b[0m  \x1b[32m✓ Configuration received and hidden.\x1b[0m");
                buffer.trim().to_string()
            } else {
                input("Provide OpenVPN configuration (Paste .ovpn content OR enter Path)")
                    .placeholder("/Users/.../vpn.ovpn or config text")
                    .interact()?
            };

            if input_val.is_empty() {
                return Err(anyhow::anyhow!("VPN configuration cannot be empty."));
            }

            let vpn_storage = std::path::Path::new("data/vpn");
            std::fs::create_dir_all(vpn_storage)?;
            let final_vpn_path: String;

            // 1. Check if it's a URL
            if input_val.starts_with("http") {
                let s = spinner();
                s.start("Downloading VPN configuration...");
                let response = reqwest::get(&input_val).await?.bytes().await?;
                let file_name = input_val.split('/').last().unwrap_or("downloaded.ovpn");
                let dest_path = vpn_storage.join(file_name);
                std::fs::write(&dest_path, response)?;
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
                let parts: Vec<&str> = normalized_input.split('/').filter(|s| !s.is_empty()).collect();
                
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
            if msg == "CDC_READY" { 
                s.stop("✓ Sync established (Real-time enabled).");
                
                // --- DOCKER FLOW ---
                if !is_docker_container {
                    let setup_docker: bool = select("Configure this entity to run in a background Docker container?")
                        .item(true, "Yes", "Generate docker-compose.yml and instructions")
                        .item(false, "No", "Continue running manually")
                        .interact()?;
                    
                    if setup_docker {
                        println!("\x1b[90m│\x1b[0m");
                        println!("\x1b[32m◆\x1b[0m  \x1b[1mDocker Setup Assistant\x1b[0m");
                        println!("\x1b[90m│\x1b[0m  \x1b[90mGenerating docker-compose.yml for '{}'... \x1b[0m", selected_entity);
                        
                        let version = env!("CARGO_PKG_VERSION");
                        let compose_content = format!(r#"services:
  bittice-{entity}:
    image: ghcr.io/julianrodelo11/bittice:v{version}
    container_name: bittice-{entity}
    restart: always
    environment:
      - BITTICE_ENTITY={entity}
      - BITTICE_HOST=0.0.0.0
    ports:
      - "3000:3000"
      - "50051:50051"
    volumes:
      - ./data:/app/data
"#, entity = selected_entity, version = version);

                        std::fs::write("docker-compose.yml", compose_content)?;
                        
                        println!("\x1b[90m│\x1b[0m");
                        println!("\x1b[32m◆\x1b[0m  \x1b[1mDone!\x1b[0m");
                        println!("\x1b[90m│\x1b[0m  \x1b[90mTo start your background engine, run:\x1b[0m");
                        println!("\x1b[90m│\x1b[0m  \x1b[1m  docker-compose up -d\x1b[0m");
                    }
                }
                break;
            }
            if msg == "CDC_DISABLED" || msg.contains("Connection timed out") || msg.contains("Access denied") {
                let reason = if msg == "CDC_DISABLED" { "CDC is not enabled on server" } else { "Could not connect to Binlog" };
                s.stop(format!("\x1b[32m◆\x1b[0m  Static data sync established ({}. Real-time updates inactive).", reason));
                
                if !is_docker_container {
                    // --- DOCKER FLOW REPEATED HERE FOR STATIC CASE ---
                    let setup_docker: bool = select("Configure this entity to run in a background Docker container?")
                        .item(true, "Yes", "Generate docker-compose.yml and instructions")
                        .item(false, "No", "Continue running manually")
                        .interact()?;
                    
                    if setup_docker {
                        println!("\x1b[90m│\x1b[0m");
                        println!("\x1b[32m◆\x1b[0m  \x1b[1mDocker Setup Assistant\x1b[0m");
                        println!("\x1b[90m│\x1b[0m  \x1b[90mGenerating docker-compose.yml for '{}'... \x1b[0m", selected_entity);
                        
                        let version = env!("CARGO_PKG_VERSION");
                        let compose_content = format!(r#"services:
  bittice-{entity}:
    image: ghcr.io/julianrodelo11/bittice:v{version}
    container_name: bittice-{entity}
    restart: always
    environment:
      - BITTICE_ENTITY={entity}
      - BITTICE_HOST=0.0.0.0
    ports:
      - "3000:3000"
      - "50051:50051"
    volumes:
      - ./data:/app/data
"#, entity = selected_entity, version = version);

                        std::fs::write("docker-compose.yml", compose_content)?;
                        
                        println!("\x1b[90m│\x1b[0m");
                        println!("\x1b[32m◆\x1b[0m  \x1b[1mDone!\x1b[0m");
                        println!("\x1b[90m│\x1b[0m  \x1b[90mTo start your background engine, run:\x1b[0m");
                        println!("\x1b[90m│\x1b[0m  \x1b[1m  docker-compose up -d\x1b[0m");
                    }
                }
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

    // Levantamos los servidores automáticamente para que el usuario pueda probar la API inmediatamente
    let server_entity = selected_entity.clone();
    tokio::spawn(async move {
        let _ = crate::server::start_all_servers(Some(server_entity)).await;
    });


    let log_path = "data/server.log";
    if std::path::Path::new(log_path).exists() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("tail -f -n 0 {} | grep --line-buffered -i -E '{}|GET|POST|PUT|DELETE|CDC|Error|Warn'", log_path, selected_entity))
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
