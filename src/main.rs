use clap::Parser;
use anyhow::{bail, Result};
use bittice::server::logging;
use tracing::info;

// Import modules from the library (package name: bittice)
use bittice::cli::{Cli, Commands};

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Production Docker (EC2): no interactive wizard or `cdc`/`setup` CLI — only the main process runs the engine.
fn docker_engine_deploy_locked() -> bool {
    std::path::Path::new("/.dockerenv").exists() && env_truthy("BITTICE_ENGINE_ONLY")
}

#[tokio::main]
async fn main() -> Result<()> {
    if !env_truthy("BITTICE_NO_RLIMIT") {
        bittice::core::fd_limits::raise_fd_limits();
    }
    let env_entity = std::env::var("BITTICE_ENTITY").ok().filter(|s| !s.trim().is_empty());
    let args_len = std::env::args().len();
    let is_docker_env = std::path::Path::new("/.dockerenv").exists();
    let is_pid1 = std::process::id() == 1;

    // `docker exec ... bittice` is not PID 1; with BITTICE_ENGINE_ONLY the real engine is already running.
    if is_docker_env
        && args_len == 1
        && env_entity.is_none()
        && !is_pid1
        && env_truthy("BITTICE_ENGINE_ONLY")
    {
        bail!(
            "El motor ya corre como proceso principal del contenedor (PID 1). \
             Para ver CDC y HTTP como en local: docker logs -f bittice   (también: tail -f data/server.log en el volumen)"
        );
    }

    // Docker main process: interactive wizard never runs here — only the engine with mounted /app/data.
    let docker_engine_mode =
        is_docker_env && args_len == 1 && env_entity.is_none() && is_pid1;

    let tracing_quiet_stdout = (!docker_engine_mode && args_len == 1 && env_entity.is_none())
        || (std::env::args().nth(1).as_deref() == Some("setup"));
    logging::init_logging(tracing_quiet_stdout);

    if docker_engine_mode {
        info!(
            "Docker: engine (PID 1) — HTTP/gRPC + CDC from /app/data. Stream logs: docker logs -f bittice"
        );
        return bittice::server::start_all_servers(None, true).await;
    }

    // CLI / Command mode (includes `bittice setup`, `bittice cdc`, …)
    if std::env::args().len() > 1 || env_entity.is_some() {
        let cli = Cli::parse();
        if docker_engine_deploy_locked() {
            bail!(
                "Este contenedor solo ejecuta el motor ya desplegado (BITTICE_ENGINE_ONLY=1): no está soportado \
                 usar aquí setup, cdc, test ni otros subcomandos. Configura y sincroniza en tu PC, vuelve a desplegar. \
                 Para ver el mismo tipo de líneas que en local (CDC, GET/POST), usa: docker logs -f bittice"
            );
        }
        match cli.command {
            Commands::Test => {
                std::env::set_var("BITTICE_DISABLE_CDC_AUTOSTART", "1");
                info!("Test mode: CDC autostart disabled. Using local data only.");
                return bittice::server::start_all_servers(None, true).await;
            }
            Commands::Setup => {
                let _ = bittice::repl::startup::run_startup_cliclack().await?;
            }
            // ... rest of commands ...

            Commands::Cdc { url, entity, database, sync_all } => {
                // Check if there's a saved config with VPN for this entity
                let config_path = bittice::core::data_paths::profile_dir(&entity).join("cdc_config.json");
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

                // Start server automatically for CDC
                let server_entity = entity.clone();
                tokio::spawn(async move {
                    let _ = bittice::server::start_all_servers(Some(server_entity), true).await;
                });

                let worker = if sync_all {
                    bittice::core::cdc::CdcWorker::new_sync_all(url, entity)
                } else {
                    let db = database.ok_or_else(|| {
                        anyhow::anyhow!("--database is required unless --sync-all is set")
                    })?;
                    bittice::core::cdc::CdcWorker::new(url, entity, db)
                };
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
        // Local workstation: interactive wizard when launched with no args.
        let _ = bittice::repl::startup::run_startup_cliclack().await?;
    }

    Ok(())
}
