use clap::Parser;
use anyhow::Result;

// Import modules from the library (package name: bittice)
use bittice::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    // If there are arguments (beyond the program name), we execute normally.
    // If not, we enter interactive mode.
    if std::env::args().len() > 1 {
        let cli = Cli::parse();
        match cli.command {
            Commands::Load { input, entity, table } => {
                bittice::commands::load::execute_load_cli(&input, &entity, &table)?;
            }
            Commands::Server { port, r#type } => {
                if r#type == "all" {
                    bittice::server::start_all_servers().await?;
                } else if r#type == "grpc" {
                    bittice::server::grpc::start_grpc_server(port).await?;
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

                    // Note: start_server binds to hardcoded 127.0.0.1:3000 in original code.
                    // We might want to pass the port in the future.
                    println!("Starting HTTP server (Port fixed to 3000 in current impl, ignore --port arg for now if different)...");
                    bittice::server::start_server(log_tx, shutdown_rx).await;
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

