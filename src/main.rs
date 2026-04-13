use clap::Parser;
use anyhow::Result;
use bittice::server::logging;
use tracing::info;

// Import modules from the library (package name: bittice)
use bittice::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    logging::init_logging();
    let env_entity = std::env::var("BITTICE_ENTITY").ok().filter(|s| !s.trim().is_empty());
    let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
    let is_pid1 = std::process::id() == 1;

    // Flow decision:
    // 1. If we are PID 1 in Docker, we ALWAYS start the servers in background first.
    // 2. If we have BITTICE_ENTITY or CLI arguments, we run in command mode.
    // 3. Otherwise, we run the interactive setup (REPL).
    
    if is_docker && is_pid1 {
        info!("Docker Environment (PID 1): Auto-starting Query Engine in background...");
        // Start servers in background immediately
        tokio::spawn(async move {
            let _ = bittice::server::start_all_servers(None).await;
        });
        
        // If we have no arguments, we still drop into the REPL so the user can configure it
        if std::env::args().len() == 1 && env_entity.is_none() {
            return bittice::repl::startup::run_startup_cliclack().await;
        }
    }

    if std::env::args().len() > 1 || env_entity.is_some() {
        if let Some(ref e) = env_entity {
            info!("Detected BITTICE_ENTITY from environment: '{}'", e);
        }
        
        let cli = Cli::parse();
        match cli.command {
            Commands::Setup => {
                let _ = bittice::repl::startup::run_startup_cliclack().await?;
            }
            Commands::Cdc { url, entity, database } => {
                // Check if there's a saved config with VPN for this entity
                let config_path = format!("data/{}/cdc_config.json", entity);
                if let Ok(content) = std::fs::read_to_string(&config_path) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(vpn_path) = json.get("vpn_file").and_then(|v| v.as_str()) {
                            info!("Auto-starting VPN from saved config for entity '{}'...", entity);
                            if let Ok(prepared) = bittice::core::vpn::VpnManager::prepare_ovpn_file(vpn_path, &url.split('@').last().unwrap_or("").split(':').next().unwrap_or("")) {
                                let _ = bittice::core::vpn::VpnManager::start(&prepared);
                            }
                        }
                    }
                }

                // Iniciar servidor automáticamente para CDC
                let server_entity = entity.clone();
                tokio::spawn(async move {
                    let _ = bittice::server::start_all_servers(Some(server_entity)).await;
                });

                let worker = bittice::core::cdc::CdcWorker::new(url, entity, database);
                worker.run().await?;
            }
            Commands::Update => {
                bittice::core::update::perform_update().await?;
            }
            Commands::Uninstall => {
                bittice::core::uninstall::perform_uninstall().await?;
            }
        }
    } else {
        // Run startup flow with Cliclack
        // This flow handles its own execution and server startup
        let _ = bittice::repl::startup::run_startup_cliclack().await?;
    }

    Ok(())
}
