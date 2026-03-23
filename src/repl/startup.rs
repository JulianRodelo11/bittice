use anyhow::Result;
use cliclack::{intro, outro, select, input, password, note, spinner, confirm};
use std::thread;
use tokio::sync::mpsc;
use crate::core::cdc::CdcWorker;
use std::process::{Command, Stdio};
use std::io::{BufRead, BufReader};

struct CdcInfo {
    host: String,
    port: String,
    user: String,
    pass: String,
    database: String,
    entity: String,
}

pub async fn run_startup_cliclack() -> Result<()> {
    intro(" bittice ")?;

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
                    entities.push(entry.file_name().to_string_lossy().to_string());
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
        let selected_entity = &entities[entity_idx];

        // Check if a docker-compose exists for this entity
        let compose_file = format!("docker-compose-bittice-{}.yml", selected_entity.to_lowercase().replace(" ", "-"));
        let mut use_docker = false;
        
        if std::path::Path::new(&compose_file).exists() {
            use_docker = confirm(format!("Docker Compose file detected for '{}'. Do you want to start it?", selected_entity))
                .initial_value(true)
                .interact()?;
        }

        if use_docker {
            let s = spinner();
            s.start(format!("Starting Docker stack for {}...", selected_entity));
            let run_status = Command::new("docker-compose")
                .args(["-f", &compose_file, "-p", &format!("bittice-{}", selected_entity), "up", "-d"])
                .status()?;
            
            if run_status.success() {
                s.stop(format!("✓ Containers for '{}' are active.", selected_entity));
                crate::server::show_banner();
                crate::server::wait_for_exit(None).await?;
            } else {
                s.stop("✗ Error starting Docker. Attempting local startup...");
                crate::server::start_all_servers().await?;
            }
        } else {
            crate::server::start_all_servers().await?;
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

    // 1. Sync Spinner
    let s = spinner();
    s.start("Starting CDC sync engine...");

    let url = format!("mysql://{}:{}@{}:{}/{}",
        cdc_info.user, cdc_info.pass,
        cdc_info.host, cdc_info.port,
        cdc_info.database);

    let (log_tx, mut log_rx) = mpsc::channel(100);

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
    while let Some(msg) = log_rx.recv().await {
        if msg == "CDC_READY" {
            bootstrap_success = true;
            break;
        }
        if msg.starts_with("CDC: Bootstrapping table") {
            s.set_message(format!("{}...", msg));
        }
    }

    if !bootstrap_success {
        s.stop("✗ Error synchronizing with the database.");
        return Err(anyhow::anyhow!("Sync failed. Verify credentials and database status."));
    }

    s.stop("✓ Sync established.");

    // 2. Ask for Docker
    let build_docker = confirm("Do you want to create a Docker image with this data?")
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
        let reader = BufReader::new(stdout);

        for line in reader.lines().flatten() {
            let short_line = if line.len() > 50 { format!("...{}", &line[line.len()-47..]) } else { line };
            s.set_message(format!("Docker: {}", short_line));
        }

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
                
                // Adjust URL for Docker: if localhost, use host.docker.internal
                let docker_mysql_host = if cdc_info.host == "localhost" || cdc_info.host == "127.0.0.1" {
                    "host.docker.internal"
                } else {
                    &cdc_info.host
                };

                let mysql_url = format!("mysql://{}:{}@{}:{}/{}", 
                    cdc_info.user, cdc_info.pass, 
                    docker_mysql_host, cdc_info.port, 
                    cdc_info.database);

                // Create a docker-compose file with TWO services (Engine + Sync)
                let compose_content = format!(r#"
version: "3.9"
services:
  engine:
    image: {0}
    container_name: {0}-engine
    ports:
      - "3000:3000"
      - "50051:50051"
    command: ["./bittice", "server", "--type", "all"]
    environment:
      - BITTICE_HOST=0.0.0.0
    volumes:
      - ./data:/app/data
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: always

  sync:
    image: {0}
    container_name: {0}-sync
    command: ["./bittice", "cdc", "--url", "{1}", "--entity", "{2}", "--database", "{3}"]
    volumes:
      - ./data:/app/data
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: always
"#, project_name, mysql_url, cdc_info.entity, cdc_info.database);

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

    // 3. Finish showing server banner
    note("Sync Complete", "Bittice engine now has access to your data.")?;
    
    let start_now = confirm("Do you want to activate the query engine right now?")
        .initial_value(true)
        .interact()?;

    if start_now {
        // If we already brought up Docker, just show banner and wait
        // If not, start local servers
        let docker_active = build_docker && std::path::Path::new(&format!("docker-compose-bittice-{}.yml", cdc_info.entity.to_lowercase().replace(" ", "-"))).exists();
        
        if docker_active {
            crate::server::show_banner();
            crate::server::wait_for_exit(None).await?;
        } else {
            crate::server::start_all_servers().await?;
        }
    } else {
        outro("Bittice is ready. You can start it later from the command line.")?;
    }
    
    Ok(())
}
