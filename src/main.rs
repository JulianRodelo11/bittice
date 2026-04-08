use clap::Parser;
use anyhow::Result;
use bittice::server::logging;
use tracing::{info, warn};

// Import modules from the library (package name: bittice)
use bittice::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    logging::init_logging();
    let env_entity = std::env::var("BITTICE_ENTITY").ok().filter(|s| !s.trim().is_empty());
    let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
    let is_pid1 = std::process::id() == 1;

    // If there are arguments (beyond the program name) OR we have BITTICE_ENTITY set, we execute server mode.
    // In Docker, we only auto-start server mode if we are the main process (PID 1).
    // Otherwise (manual exec), we enter interactive setup mode.
    if std::env::args().len() > 1 || env_entity.is_some() || (is_docker && is_pid1) {
        if let Some(ref e) = env_entity {
            info!("Detected BITTICE_ENTITY from environment: '{}'", e);
        }

        let cli = if std::env::args().len() > 1 {
            Cli::parse()
        } else {
            // Default to 'server' mode if no args but in Docker (as PID 1) or ENV is set
            let mode_msg = if is_docker { "Docker environment (Main Process)" } else { "BITTICE_ENTITY environment variable" };
            info!("No CLI arguments provided, auto-starting server due to {}.", mode_msg);
            Cli {
                command: Commands::Server { 
                    port: 50051, 
                    r#type: "all".to_string(), 
                    entity: env_entity.clone() 
                }
            }
        };

        match cli.command {
            Commands::Server { port, r#type, entity } => {
                let final_entity = entity.or(env_entity);
                
                if let Some(ref e) = final_entity {
                    info!("Starting server with entity filter: '{}'", e);
                } else {
                    info!("Starting server with NO entity filter (loading all)");
                }

                if r#type == "all" {
                    bittice::server::start_all_servers(final_entity).await?;
                } else if r#type == "grpc" {
                    bittice::server::grpc::start_grpc_server(port, final_entity).await?;
                } else {
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    
                    // Handle Ctrl+C
                    tokio::spawn(async move {
                        tokio::signal::ctrl_c().await.unwrap();
                        warn!("Shutting down server...");
                        let _ = shutdown_tx.send(());
                    });

                    info!("Starting HTTP server (Port fixed to 3000 in current impl)...");
                    let table_manager = std::sync::Arc::new(bittice::server::table_manager::TableManager::new());
                    bittice::server::start_server(table_manager, final_entity, shutdown_rx).await;
                }
            }
            Commands::Cdc { url, entity, database } => {
                let worker = bittice::core::cdc::CdcWorker::new(url, entity, database);
                worker.run().await?;
            }
        }
    } else {
        // Run startup flow with Cliclack
        // This flow handles its own execution and server startup
        let _ = bittice::repl::startup::run_startup_cliclack().await?;
    }

    Ok(())
}
