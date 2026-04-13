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
    // 1. If we have BITTICE_ENTITY or CLI arguments, we run in command/server mode.
    // 2. If we are PID 1 in Docker and have NO arguments, we auto-start the server for all entities.
    // 3. Otherwise, we run the interactive setup (REPL).
    if std::env::args().len() > 1 || env_entity.is_some() || (is_docker && is_pid1) {
        if std::env::args().len() == 1 && env_entity.is_none() && is_docker && is_pid1 {
            // Check if there are any configured entities
            let has_entities = std::fs::read_dir("data").map(|d| d.flatten().any(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))).unwrap_or(false);
            
            if !has_entities {
                // No entities to serve, run the wizard even if PID 1
                return bittice::repl::startup::run_startup_cliclack().await;
            }

            info!("No CLI arguments provided, auto-starting server in Docker (PID 1).");
            return bittice::server::start_all_servers(None).await;
        }

        if let Some(ref e) = env_entity {
            info!("Detected BITTICE_ENTITY from environment: '{}'", e);
        }

        // If no args but env_entity is set, we use start_all_servers with that entity
        if std::env::args().len() == 1 && env_entity.is_some() {
            return bittice::server::start_all_servers(env_entity).await;
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
