use clap::Parser;
use anyhow::Result;

// Import modules from the library (package name: bittice)
use bittice::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    let env_entity = std::env::var("BITTICE_ENTITY").ok().filter(|s| !s.trim().is_empty());

    // If there are arguments (beyond the program name) OR we have BITTICE_ENTITY set, we execute server mode.
    // If not, we enter interactive mode.
    if std::env::args().len() > 1 || env_entity.is_some() {
        if let Some(ref e) = env_entity {
            println!("[MAIN] Detected BITTICE_ENTITY from environment: '{}'", e);
        }

        let cli = if std::env::args().len() > 1 {
            Cli::parse()
        } else {
            // Default to 'server' mode if no args but ENV is set
            println!("[MAIN] No CLI arguments provided, auto-starting server due to BITTICE_ENTITY.");
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
                    println!("[MAIN] Starting server with entity filter: '{}'", e);
                } else {
                    println!("[MAIN] Starting server with NO entity filter (loading all)");
                }

                if r#type == "all" {
                    bittice::server::start_all_servers(final_entity).await?;
                } else if r#type == "grpc" {
                    bittice::server::grpc::start_grpc_server(port, final_entity).await?;
                } else {
                    // Setup for HTTP server (needs log channel and shutdown signal)
                    let (log_tx, mut log_rx) = tokio::sync::mpsc::channel(100);
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    
                    // Task to print logs
                    tokio::spawn(async move {
                        while let Some(msg) = log_rx.recv().await {
                            println!("{}", msg);
                        }
                    });

                    // Handle Ctrl+C
                    tokio::spawn(async move {
                        tokio::signal::ctrl_c().await.unwrap();
                        println!("Shutting down server...");
                        let _ = shutdown_tx.send(());
                    });

                    // Note: start_server binds to hardcoded 0.0.0.0:3000 in original code.
                    // We might want to pass the port in the future.
                    println!("Starting HTTP server (Port fixed to 3000 in current impl, ignore --port arg for now if different)...");
                    let table_manager = std::sync::Arc::new(bittice::server::table_manager::TableManager::new());
                    bittice::server::start_server(log_tx, table_manager, final_entity, shutdown_rx).await;
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

