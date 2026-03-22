use clap::Parser;
use anyhow::Result;

// Importar módulos desde la librería (nombre del paquete: bittice)
use bittice::cli::{Cli, Commands};

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
            Commands::Query { entity, table, limit } => {
                let base_path = std::path::Path::new("data").join(&entity);
                let table_obj = bittice::core::storage::table::Table::open(&base_path, &table)?;
                
                // Si no se especificaron campos, detectar todos los .dat disponibles en el primer segmento
                let fields = {
                    let mut detected = Vec::new();
                    let seg_path = table_obj.base_path.join("segments").join("seg_0000");
                    if let Ok(entries) = std::fs::read_dir(seg_path) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.ends_with(".dat") && !name.starts_with("bitmaps_") {
                                detected.push(name.replace(".dat", ""));
                            }
                        }
                    }
                    detected.sort();
                    detected
                };
                
                let result = table_obj.search(&fields, &[], &bittice::core::types::LogicalOp::And, &[], &[], limit, 0)?;
                println!("Query results for {}/{} (total found: {}):", entity, table, result.total_found);
                println!("Headers: {:?}", result.headers);
                for row in result.rows {
                    println!("{:?}", row);
                }
            }
        }
    } else {
        // Run startup flow with Cliclack
        // This flow handles its own execution and server startup
        let _ = bittice::repl::startup::run_startup_cliclack().await?;
    }

    Ok(())
}

