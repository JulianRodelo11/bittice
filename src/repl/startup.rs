use anyhow::Result;
use cliclack::{intro, outro, select, input, password, note, spinner, confirm};
use serde::{Deserialize, Serialize};
use std::thread;
use tokio::sync::{mpsc, oneshot};
use crate::core::cdc::CdcWorker;
use std::process::{Command, Stdio};
use std::io::{BufRead, BufReader};
use tokio::time::{timeout, Duration};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct CdcInfo {
    host: String,
    port: String,
    user: String,
    pass: String,
    database: String,
    entity: String,
}

fn save_cdc_config(info: &CdcInfo) -> anyhow::Result<()> {
    let path = std::path::Path::new("data").join(&info.entity).join("cdc_config.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(info)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn load_cdc_config(entity: &str) -> anyhow::Result<CdcInfo> {
    let path = std::path::Path::new("data").join(entity).join("cdc_config.json");
    let content = std::fs::read_to_string(path)?;
    let info: CdcInfo = serde_json::from_str(&content)?;
    Ok(info)
}

async fn is_docker_stack_running(project_name: &str) -> bool {
    let fut = tokio::process::Command::new("docker-compose")
        .args(["-p", project_name, "ps", "--format", "json"])
        .output();
    match timeout(Duration::from_secs(12), fut).await {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains("\"State\":\"running\"") || stdout.contains("running")
        }
        _ => false,
    }
}

pub async fn run_startup_cliclack() -> Result<()> {
    intro("Bittice")?;

    let option: u8 = select("Select operation mode")
        .item(0, "Connect and synchronize to a database", "Configure a new MySQL CDC connection")
        .item(1, "Use Bittice with an already connected database", "Enter the query dashboard directly")
        .interact()?;

    if option == 1 {
        // List available entities
        let data_dir = std::path::Path::new("data");
        let mut entities = Vec::new();
        if let Ok(entries) = std::fs::read_dir(data_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') {
                        entities.push(name);
                    }
                }
            }
        }

        if entities.is_empty() {
            note("No entities found", "You must connect at least one database first.")?;
            return Ok(());
        }

        let mut select_entity = select("Select the Entity you want to use");
        for (i, entity) in entities.iter().enumerate() {
            select_entity = select_entity.item(i, entity, "");
        }
        let entity_idx = select_entity.interact()?;
        let selected_entity = entities[entity_idx].clone();

        // 1. Try to load saved CDC config to offer real-time sync retry
        if let Ok(mut config) = load_cdc_config(&selected_entity) {
            let retry_cdc = confirm("Real-time sync may be inactive. Attempt to re-establish Binlog connection?")
                .initial_value(false)
                .interact()?;
            
            if retry_cdc {
                // Let user update password or user just in case it was a permission issue
                let update_creds = confirm("Do you want to update connection credentials (User/Password)?")
                    .initial_value(false)
                    .interact()?;
                
                if update_creds {
                    config.user = input("User").default_input(&config.user).interact()?;
                    config.pass = password("Password").mask('*').interact()?;
                    let _ = save_cdc_config(&config);
                }

                let s = spinner();
                s.start("Attempting to re-establish CDC...");
                
                let url = format!("mysql://{}:{}@{}:{}/{}", config.user, config.pass, config.host, config.port, config.database);
                let (log_tx, mut log_rx) = mpsc::channel::<String>(100);
                
                let worker_url = url.clone();
                let worker_entity = config.entity.clone();
                let worker_db = config.database.clone();

                // Dedicated runtime in a thread: CdcWorker::run is not Send (std::sync::RwLock across await).
                let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                let mut cancel_once = Some(cancel_tx);
                thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().expect("CDC runtime");
                    rt.block_on(async move {
                        let worker =
                            CdcWorker::with_log(worker_url, worker_entity, worker_db, Some(log_tx));
                        tokio::select! {
                            res = worker.run() => {
                                let _ = res;
                            }
                            _ = cancel_rx => {}
                        }
                    });
                });

                let mut finished = false;
                while let Some(msg) = log_rx.recv().await {
                    if msg == "CDC_READY" {
                        s.stop("✓ Real-time sync established successfully.");
                        if let Some(tx) = cancel_once.take() {
                            let _ = tx.send(());
                        }
                        finished = true;
                        break;
                    }
                    if msg == "CDC_DISABLED" {
                        s.stop("⚠ Could not enable real-time sync. Check your MySQL Binlog permissions.");
                        if let Some(tx) = cancel_once.take() {
                            let _ = tx.send(());
                        }
                        finished = true;
                        break;
                    }
                    if let Some(err) = msg.strip_prefix("CDC_ERROR: ") {
                        s.stop(format!("✗ Error: {}", err));
                        if let Some(tx) = cancel_once.take() {
                            let _ = tx.send(());
                        }
                        finished = true;
                        break;
                    }
                }
                if !finished {
                    if let Some(tx) = cancel_once.take() {
                        let _ = tx.send(());
                    }
                    s.stop("✗ Error: CDC stopped before confirming binlog sync.");
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        // Check if a docker-compose exists for this entity
        let project_name = format!("bittice-{}", selected_entity).to_lowercase().replace(" ", "-");
        let compose_file = format!("docker-compose-{}.yml", project_name);
        let mut use_docker = false;
        
        if std::path::Path::new(&compose_file).exists() {
            if is_docker_stack_running(&project_name).await {
                note("Docker", format!("Containers for '{}' are already running.", selected_entity))?;
                crate::server::show_banner();
                
                // Integrated Log Viewer for already running container
                note("Logs", "Streaming real-time logs (Press Ctrl+C to stop)...")?;
                let mut child = Command::new("docker")
                    .args(["logs", "-f", "--tail", "20", &format!("{}-container", project_name)])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()?;
                
                let stdout = child.stdout.take().unwrap();
                let reader = BufReader::new(stdout);
                for line in reader.lines().flatten() {
                    println!("{}", line);
                }
                
                let _ = child.wait();
                return Ok(());
            }

            use_docker = confirm(format!("Docker Compose stack detected for '{}'. Start containers?", selected_entity))
                .initial_value(true)
                .interact()?;
        }

        if use_docker {
            // Cleanup old local processes before starting Docker
            let _ = Command::new("lsof").args(["-i", ":3000", "-t"]).output().and_then(|out| {
                let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !pid.is_empty() { let _ = Command::new("kill").args(["-9", &pid]).status(); }
                Ok(())
            });
            let _ = Command::new("lsof").args(["-i", ":50051", "-t"]).output().and_then(|out| {
                let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !pid.is_empty() { let _ = Command::new("kill").args(["-9", &pid]).status(); }
                Ok(())
            });

            let s = spinner();
            s.start(format!("Restarting Docker engine for '{}' with latest changes...", selected_entity));
            let run_status = Command::new("docker-compose")
                .args(["-f", &compose_file, "-p", &project_name, "up", "--build", "-d"])
                .status()?;
            
            if run_status.success() {
                s.stop(format!("✓ Docker engine for '{}' is running.", selected_entity));
                crate::server::show_banner();
                
                // Integrated Log Viewer
                note("Logs", "Streaming real-time logs (Press Ctrl+C to stop)...")?;
                let mut child = Command::new("docker")
                    .args(["logs", "-f", "--tail", "20", &format!("{}-container", project_name)])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()?;
                
                let stdout = child.stdout.take().unwrap();
                let reader = BufReader::new(stdout);
                for line in reader.lines().flatten() {
                    println!("{}", line);
                }
                
                let _ = child.wait();
            } else {
                s.stop("✗ Error starting Docker. Attempting local startup...");
                crate::server::start_all_servers(Some(selected_entity)).await?;
            }
        } else {
            crate::server::start_all_servers(Some(selected_entity)).await?;
        }
        
        return Ok(());
    }

    // Connection flow
    let host: String = input("MySQL Host").default_input("localhost").interact()?;
    let port: String = input("Port").default_input("3306").interact()?;
    let user: String = input("User").default_input("root").interact()?;
    let pass: String = password("Password").mask('*').interact()?;
    let database: String = input("Database to synchronize").placeholder("sakila").interact()?;
    let entity: String = input("Entity name in Bittice").default_input(&database).interact()?;

    let cdc_info = CdcInfo {
        host,
        port,
        user,
        pass,
        database,
        entity,
    };

    let _ = save_cdc_config(&cdc_info);

    // 1. Sync Spinner
    let s = spinner();
    s.start("Starting CDC sync engine...");

    let url = format!("mysql://{}:{}@{}:{}/{}",
        cdc_info.user, cdc_info.pass,
        cdc_info.host, cdc_info.port,
        cdc_info.database);

    let (log_tx, mut log_rx) = mpsc::channel::<String>(100);

    let worker_url = url.clone();
    let worker_entity = cdc_info.entity.clone();
    let worker_db = cdc_info.database.clone();

    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let worker = CdcWorker::with_log(worker_url, worker_entity, worker_db, Some(log_tx));
        let _ = rt.block_on(worker.run());
    });

    // Wait for bootstrap to finish
    let mut bootstrap_success = false;
    let mut error_msg = String::from("Sync failed. Verify credentials and database status.");
    
    let mut spinner_stopped = false;
    while let Some(msg) = log_rx.recv().await {
        if msg == "CDC_READY" {
            bootstrap_success = true;
            break;
        }
        if msg == "CDC_DISABLED" {
            bootstrap_success = true;
            s.stop("⚠ Sync partially established (static data only).");
            spinner_stopped = true;
            break;
        }
        if let Some(err) = msg.strip_prefix("CDC_ERROR: ") {
            error_msg = err.to_string();
            break;
        }
        if msg.starts_with("CDC: Bootstrapping table") {
            s.set_message(format!("{}...", msg));
        }
    }

    if !bootstrap_success {
        s.stop(format!("✗ Error: {}", error_msg));
        return Err(anyhow::anyhow!(error_msg));
    }

    if !spinner_stopped {
        s.stop("✓ Sync established.");
    }

    // Check if we are already running in Docker
    let is_docker = std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok();
    let mut build_docker = false;

    if !is_docker {
        // 2. Ask for Docker (Only if NOT already in docker)
        build_docker = confirm("Do you want to create a Docker image with this data?")
            .initial_value(true)
            .interact()?;

        if build_docker {
            let s = spinner();
            s.start("Building Docker image...");
            
            // Docker requires lowercase names
            let image_name = format!("bittice-{}", cdc_info.entity).to_lowercase().replace(" ", "-");
            
            let mut child = Command::new("docker")
                .args(["build", "-t", &image_name, "."])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            
            let s_clone = s.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().flatten() {
                    let msg = if line.len() > 60 { format!("...{}", &line[line.len()-57..]) } else { line };
                    s_clone.set_message(format!("Docker: {}", msg));
                }
            });

            let s_clone_err = s.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().flatten() {
                    let msg = if line.len() > 60 { format!("...{}", &line[line.len()-57..]) } else { line };
                    s_clone_err.set_message(format!("Docker: {}", msg));
                }
            });

            let status = child.wait()?;
            if status.success() {
                s.stop(format!("✓ Image '{}' created successfully.", image_name));
                
                // NEW: Ask if user wants to start the container
                let run_container = confirm(format!("Do you want to start a container with image '{}'?", image_name))
                    .initial_value(true)
                    .interact()?;
                
                if run_container {
                    let s = spinner();
                    s.start("Generating Docker Compose stack...");
                    let project_name = format!("bittice-{}", cdc_info.entity).to_lowercase().replace(" ", "-");
                    let compose_file = format!("docker-compose-{}.yml", project_name);

                    // ALWAYS update the docker-compose file to ensure latest filters/env vars are applied
                    let image_name = project_name.clone();
                    let compose_content = format!(r#"services:
  bittice:
    build: .
    image: {0}
    container_name: {0}-container
    ports:
      - "3000:3000"
      - "50051:50051"
    environment:
      - BITTICE_HOST=0.0.0.0
      - BITTICE_ENTITY={1}
    volumes:
      - ./data:/app/data
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: always
"#, image_name, cdc_info.entity.to_lowercase().trim());

                    std::fs::write(&compose_file, compose_content)?;

                    s.set_message("Bringing up services with Docker Compose...");
                    
                    // Run docker-compose up -d
                    let run_status = Command::new("docker-compose")
                        .args([
                            "-f", &compose_file,
                            "-p", &project_name,
                            "up", "-d"
                        ])
                        .status()?;

                    if run_status.success() {
                        s.stop(format!("✓ Stack '{}' started correctly.", project_name));
                        note("Docker Compose", format!("You will now see the group '{}' in Docker Desktop with the Bittice engine inside.\nURL: http://localhost:3000", project_name))?;
                    } else {
                        s.stop("✗ Error starting docker-compose. Make sure you have it installed.");
                    }
                }
            } else {
                s.stop("✗ Error building Docker image (check that the name is valid).");
            }
        }
    } else {
        note("Docker Environment", "Configuration saved. Bittice is already running in Docker.")?;
    }

    // 3. Finish showing server banner
    note("Sync Complete", "Bittice engine now has access to your data.")?;
    
    let start_now = confirm("Do you want to activate the query engine right now?")
        .initial_value(true)
        .interact()?;

    if start_now {
        let project_name = format!("bittice-{}", cdc_info.entity).to_lowercase().replace(" ", "-");
        let compose_file = format!("docker-compose-{}.yml", project_name);
        let docker_active = !is_docker && build_docker && std::path::Path::new(&compose_file).exists();

        if docker_active {
            // Check if container is already running
            let check_status = Command::new("docker")
                .args(["ps", "-q", "-f", &format!("name={}-container", project_name)])
                .output();
            
            let is_running = check_status.ok().map(|out| !out.stdout.is_empty()).unwrap_or(false);

            if !is_running {
                // 1. Cleanup old local processes
                let _ = Command::new("lsof").args(["-i", ":3000", "-t"]).output().and_then(|out| {
                    let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !pid.is_empty() { let _ = Command::new("kill").args(["-9", &pid]).status(); }
                    Ok(())
                });
                let _ = Command::new("lsof").args(["-i", ":50051", "-t"]).output().and_then(|out| {
                    let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !pid.is_empty() { let _ = Command::new("kill").args(["-9", &pid]).status(); }
                    Ok(())
                });

                // 2. Restart Docker Stack with build
                let s = spinner();
                s.start("Restarting Docker engine with latest changes...");
                
                // Explicitly DOWN first to avoid stale containers
                let _ = Command::new("docker-compose")
                    .args(["-f", &compose_file, "-p", &project_name, "down"])
                    .status();

                let _ = Command::new("docker-compose")
                    .args(["-f", &compose_file, "-p", &project_name, "up", "--build", "-d"])
                    .status();
                s.stop("✓ Docker engine is running.");
            } else {
                note("Docker", "Container is already active and running.")?;
            }

            crate::server::show_banner();
            
            // 3. Integrated Log Viewer
            note("Logs", "Streaming real-time logs (Press Ctrl+C to stop)...")?;
            let mut child = Command::new("docker")
                .args(["logs", "-f", &format!("{}-container", project_name)])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            
            let stdout = child.stdout.take().unwrap();
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                println!("{}", line);
            }
            
            let _ = child.wait();
        } else {
            crate::server::start_all_servers(Some(cdc_info.entity.to_lowercase())).await?;
        }
    } else {
        outro("Bittice is ready. You can start it later from the command line.")?;
    }
    
    Ok(())
}
