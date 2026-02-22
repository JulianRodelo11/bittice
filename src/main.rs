use clap::Parser;
use anyhow::Result;

// Importar módulos desde la librería (nombre del paquete: bittice)
use bittice::cli::{Cli, Commands};
use bittice::repl;

#[tokio::main]
async fn main() -> Result<()> {
    // Si hay argumentos (más allá del nombre del programa), ejecutamos normalmente.
    // Si no, entramos al modo interactivo.
    if std::env::args().len() > 1 {
        let cli = Cli::parse();
        match cli.command {
            Commands::Load { input, entity, table } => {
                bittice::commands::load::execute_load_cli(&input, &entity, &table)?;
            }
            Commands::Server { port, r#type } => {
                if r#type == "grpc" {
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
        }
    } else {
        // Run REPL in a blocking way on this thread.
        // Since we are in a tokio runtime, we should use spawn_blocking if it was heavy,
        // but since it takes over the whole process loop, calling it directly is fine
        // as long as we don't expect other async tasks to run in background.
        repl::run_interactive()?;
    }

    Ok(())
}

