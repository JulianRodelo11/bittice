use anyhow::Result;
use cliclack::{intro, outro, select, input, password, note, spinner};
use serde::{Deserialize, Serialize};
use std::thread;
use tokio::sync::mpsc;
use crate::core::cdc::CdcWorker;
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
                    if !name.starts_with('.') && name != "goparking" { 
                        entities.push(name);
                    }
                }
            }
        }
        
        if std::path::Path::new("data/goparking").exists() && !entities.contains(&"goparking".to_string()) {
            entities.push("goparking".to_string());
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
        let database: String = input("Database to synchronize").placeholder("sakila").interact()?;
        let entity: String = input("Entity name in Bittice").default_input(&database).interact()?;

        let cdc_info = CdcInfo {
            host,
            port,
            user,
            pass,
            database,
            entity,
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
                s.stop("✓ Sync established.");
                break; 
            }
            if msg == "CDC_DISABLED" {
                s.stop("\x1b[32m◆\x1b[0m  Static data sync established (Real-time updates inactive).");
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
            // Simple message for the spinner
            s.set_message(msg);
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

    let log_path = "data/server.log";
    if std::path::Path::new(log_path).exists() {
        // -n 0 starts tailing from the end to avoid cluttering with old logs
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
