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

fn main() -> Result<()> {
    // rustls 0.23 panics on the first TLS handshake when multiple crypto
    // providers are linked but none is registered as process default. Both
    // `aws-lc-rs` and `ring` end up in the dep tree (reqwest + mysql_async).
    // Pick one explicitly here so any later TLS user (heartbeat, self_health,
    // CDC against a TLS-required source) just works. Idempotent — the second
    // caller gets Err which we discard.
    let _ = rustls::crypto::ring::default_provider().install_default();

    if !env_truthy("BITTICE_NO_RLIMIT") {
        bittice::core::fd_limits::raise_fd_limits();
    }

    // Cap the blocking thread pool used by tokio::task::spawn_blocking.
    // The default (512) causes scheduler thrashing on small burstable instances
    // when many CPU-bound queries run concurrently. One saved-op read can
    // consume two blocking slots (search + materialize), so 16 slots supports
    // roughly eight concurrent queries without oversubscribing two CPU cores.
    let max_blocking_threads = std::env::var("BITTICE_MAX_BLOCKING_THREADS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(16);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(max_blocking_threads)
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build Tokio runtime: {e}"))?;

    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
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
            Commands::Cdc { url, entity, database, sync_all } => {
                // Bittice does not manage tunnels: users with VPN-only MySQL
                // must already have their OS-native VPN client up before running this.

                // Start server automatically for CDC.
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
            Commands::MigrateExactIndex {
                entity,
                table,
                field,
                all,
                dry_run,
                keep_backup,
                force,
            } => {
                if all {
                    let data_root = bittice::core::data_paths::resolved_data_root();
                    let results = bittice::core::migrate_exact_index::migrate_all(
                        &data_root,
                        dry_run,
                        keep_backup,
                        force,
                    );
                    if results.iter().any(|r| r.error.is_some()) {
                        std::process::exit(1);
                    }
                } else {
                    match (entity, table) {
                        (Some(ent), Some(tbl)) => {
                            let table_dir =
                                bittice::core::data_paths::mirror_entity_dir(&ent).join(&tbl);
                            if !table_dir.exists() {
                                anyhow::bail!(
                                    "Directorio de tabla no encontrado: {}",
                                    table_dir.display()
                                );
                            }
                            let results = bittice::core::migrate_exact_index::migrate_table(
                                &ent,
                                &tbl,
                                &table_dir,
                                field.as_deref(),
                                dry_run,
                                keep_backup,
                                force,
                            );
                            bittice::core::migrate_exact_index::print_table_results(&results);
                            if results.iter().any(|r| r.error.is_some()) {
                                std::process::exit(1);
                            }
                        }
                        _ => {
                            anyhow::bail!(
                                "Especificar <ENTITY> <TABLE> o usar --all. \
                                 Ejemplo: bittice migrate-exact-index mi_entity mi_tabla [campo]"
                            );
                        }
                    }
                }
            }
            Commands::CompactMirror { entity, table, all_tables } => {
                if all_tables {
                    let results = bittice::core::mirror_maintenance::compact_mirror_entity(&entity)?;
                    for (name, removed) in &results {
                        println!("{entity}/{name}: compacted {removed} segment(s)");
                    }
                } else {
                    let tbl = table.ok_or_else(|| {
                        anyhow::anyhow!("Specify <TABLE> or use --all-tables")
                    })?;
                    let removed =
                        bittice::core::mirror_maintenance::compact_mirror_table(&entity, &tbl)?;
                    println!("{entity}/{tbl}: compacted {removed} segment(s)");
                }
            }
            Commands::CheckMirror { entity, table, revalidate, json } => {
                let opts = bittice::core::mirror_consistency::CheckMirrorOptions {
                    entity_filter: entity,
                    table_filter: if table.is_empty() {
                        None
                    } else {
                        Some(table)
                    },
                    revalidate,
                };
                let rows =
                    bittice::core::mirror_consistency::check_mirror_consistency(opts).await?;
                if rows.is_empty() {
                    anyhow::bail!(
                        "no bootstrapped tables to check — sync first or adjust --entity / --table"
                    );
                }
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    println!(
                        "{:<20} {:<40} {:>8} {:>8} {:>8} {}",
                        "PROFILE", "TABLE", "MYSQL", "MIRROR", "DIFF", "OK"
                    );
                    for r in &rows {
                        let status = if r.ok { "OK" } else { "DRIFT" };
                        println!(
                            "{:<20} {:<40} {:>8} {:>8} {:>8} {}",
                            r.profile, r.table, r.source_count, r.mirror_count, r.diff, status
                        );
                    }
                    let drift = rows.iter().filter(|r| !r.ok).count();
                    println!();
                    if drift == 0 {
                        println!("{} table(s) — all counts match", rows.len());
                    } else {
                        println!(
                            "{drift}/{} table(s) with drift (positive DIFF = mirror missing rows)",
                            rows.len()
                        );
                    }
                }
                if rows.iter().any(|r| !r.ok) {
                    std::process::exit(1);
                }
            }
            Commands::MigratePrimaryIndex { entity, table, all, dry_run, keep_backup, force } => {
                if all {
                    let data_root = bittice::core::data_paths::resolved_data_root();
                    let results = bittice::core::migrate_primary_index::migrate_all(
                        &data_root, dry_run, keep_backup, force,
                    );
                    let has_errors = results.iter().any(|r| r.error.is_some());
                    if has_errors {
                        std::process::exit(1);
                    }
                } else {
                    match (entity, table) {
                        (Some(ent), Some(tbl)) => {
                            let table_dir = bittice::core::data_paths::mirror_entity_dir(&ent).join(&tbl);
                            if !table_dir.exists() {
                                anyhow::bail!("Directorio de tabla no encontrado: {}", table_dir.display());
                            }
                            let result = bittice::core::migrate_primary_index::migrate_table(
                                &ent, &tbl, &table_dir, dry_run, keep_backup, force,
                            );
                            result.print_report();
                            if result.error.is_some() {
                                std::process::exit(1);
                            }
                        }
                        _ => {
                            anyhow::bail!(
                                "Especificar <ENTITY> <TABLE> o usar --all. \
                                 Ejemplo: bittice migrate-primary-index mi_entity mi_tabla"
                            );
                        }
                    }
                }
            }
        }
    } else {
        // Local workstation: interactive wizard when launched with no args.
        let _ = bittice::repl::startup::run_startup_cliclack().await?;
    }

    Ok(())
}
