use clap::Parser;
use anyhow::Result;
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

#[tokio::main]
async fn main() -> Result<()> {
    let env_entity = std::env::var("BITTICE_ENTITY").ok().filter(|s| !s.trim().is_empty());
    let args_len = std::env::args().len();
    let is_docker_env = std::path::Path::new("/.dockerenv").exists();
    let is_pid1 = std::process::id() == 1;

    // EC2/server images: run only the engine against mounted `data/` (no connect/sync wizard).
    // PID 1 keeps backwards compatibility; BITTICE_ENGINE_ONLY is set in official compose files when PID≠1.
    let docker_engine_mode = is_docker_env
        && args_len == 1
        && env_entity.is_none()
        && !env_truthy("BITTICE_CLI_MENU")
        && (is_pid1 || env_truthy("BITTICE_ENGINE_ONLY"));

    let tracing_quiet_stdout = (!docker_engine_mode && args_len == 1 && env_entity.is_none())
        || (std::env::args().nth(1).as_deref() == Some("setup"));
    logging::init_logging(tracing_quiet_stdout);

    if docker_engine_mode {
        info!(
            "Docker: engine-only startup — HTTP/gRPC + CDC from existing data (/app/data). \
             Interactive sync/setup runs on your workstation (`bittice` or `bittice setup`), not in this container."
        );
        return bittice::server::start_all_servers(None, true).await;
    }

    // CLI / Command mode (includes `bittice setup`, `bittice cdc`, …)
    if std::env::args().len() > 1 || env_entity.is_some() {
        let cli = Cli::parse();
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
