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

    let option: u8 = select("Seleccione el modo de operación")
        .item(0, "Conectar y sincronizar a una base de datos", "Configura una nueva conexión MySQL CDC")
        .item(1, "Usar Bittice a una base de datos ya conectada", "Entra directamente al panel de consultas")
        .interact()?;

    if option == 1 {
        // Listar entidades disponibles
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
            note("No se encontraron entidades", "Debes conectar al menos una base de datos primero.")?;
            return Ok(());
        }

        let mut select_entity = select("Seleccione la Entidad que desea usar");
        for (i, entity) in entities.iter().enumerate() {
            select_entity = select_entity.item(i, entity, "");
        }
        let entity_idx = select_entity.interact()?;
        let selected_entity = &entities[entity_idx];

        // Verificar si existe un docker-compose para esta entidad
        let compose_file = format!("docker-compose-bittice-{}.yml", selected_entity.to_lowercase().replace(" ", "-"));
        let mut use_docker = false;
        
        if std::path::Path::new(&compose_file).exists() {
            use_docker = confirm(format!("Se detectó un archivo Docker Compose para '{}'. ¿Deseas iniciarlo?", selected_entity))
                .initial_value(true)
                .interact()?;
        }

        if use_docker {
            let s = spinner();
            s.start(format!("Iniciando Docker stack para {}...", selected_entity));
            let run_status = Command::new("docker-compose")
                .args(["-f", &compose_file, "-p", &format!("bittice-{}", selected_entity), "up", "-d"])
                .status()?;
            
            if run_status.success() {
                s.stop(format!("✓ Contenedores de '{}' activos.", selected_entity));
                crate::server::show_banner();
                crate::server::wait_for_exit(None).await?;
            } else {
                s.stop("✗ Error al iniciar Docker. Intentando inicio local...");
                crate::server::start_all_servers().await?;
            }
        } else {
            crate::server::start_all_servers().await?;
        }
        
        return Ok(());
    }

    // Flujo de conexión
    let host: String = input("MySQL Host").default_input("localhost").interact()?;
    let port: String = input("Puerto").default_input("3306").interact()?;
    let user: String = input("Usuario").default_input("root").interact()?;
    let pass: String = password("Contraseña").mask('*').interact()?;
    let database: String = input("Base de datos a sincronizar").placeholder("sakila").interact()?;
    let entity: String = input("Nombre de la Entidad en Bittice").default_input(&database).interact()?;

    let cdc_info = CdcInfo {
        host,
        port,
        user,
        pass,
        database,
        entity,
    };

    // 1. Spinner de Sincronización
    let s = spinner();
    s.start("Iniciando motor de sincronización CDC...");

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

    // Esperar a que el bootstrap termine
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
        s.stop("✗ Error al sincronizar con la base de datos.");
        return Err(anyhow::anyhow!("Sincronización fallida. Verifica las credenciales y el estado de la base de datos."));
    }

    s.stop("✓ Sincronización establecida.");

    // 2. Preguntar por Docker
    let build_docker = confirm("¿Deseas crear una imagen de Docker con estos datos?")
        .initial_value(true)
        .interact()?;

    if build_docker {
        let s = spinner();
        s.start("Construyendo imagen Docker...");
        
        // Docker requiere nombres en minúsculas
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
            s.stop(format!("✓ Imagen '{}' creada con éxito.", image_name));
            
            // NUEVO: Preguntar si desea iniciar el contenedor
            let run_container = confirm(format!("¿Deseas iniciar un contenedor con la imagen '{}'?", image_name))
                .initial_value(true)
                .interact()?;
            
            if run_container {
                let s = spinner();
                s.start("Generando stack de Docker Compose...");
                
                let project_name = format!("bittice-{}", cdc_info.entity).to_lowercase().replace(" ", "-");
                let compose_file = format!("docker-compose-{}.yml", project_name);
                
                // Ajustar URL para Docker: si es localhost, usar host.docker.internal
                let docker_mysql_host = if cdc_info.host == "localhost" || cdc_info.host == "127.0.0.1" {
                    "host.docker.internal"
                } else {
                    &cdc_info.host
                };

                let mysql_url = format!("mysql://{}:{}@{}:{}/{}", 
                    cdc_info.user, cdc_info.pass, 
                    docker_mysql_host, cdc_info.port, 
                    cdc_info.database);

                // Crear un archivo docker-compose con DOS servicios (Engine + Sync)
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

                s.set_message("Levantando servicios con Docker Compose...");
                
                // Ejecutar docker-compose up -d
                let run_status = Command::new("docker-compose")
                    .args([
                        "-f", &compose_file,
                        "-p", &project_name,
                        "up", "-d"
                    ])
                    .status()?;

                if run_status.success() {
                    s.stop(format!("✓ Stack '{}' iniciado correctamente.", project_name));
                    note("Docker Compose", format!("Ahora verás el grupo '{}' en Docker Desktop con el motor de Bittice dentro.\nURL: http://localhost:3000", project_name))?;
                } else {
                    s.stop("✗ Error al iniciar docker-compose. Asegúrate de tenerlo instalado.");
                }
            }
        } else {
            s.stop("✗ Error al construir la imagen de Docker (revisa que el nombre sea válido).");
        }
    }

    // 3. Finalizar mostrando el banner del servidor
    note("Sincronización Completa", "El motor de Bittice ya tiene acceso a tus datos.")?;
    
    let start_now = confirm("¿Deseas activar el motor de consultas ahora mismo?")
        .initial_value(true)
        .interact()?;

    if start_now {
        // Si ya levantamos Docker, solo mostramos el banner y esperamos
        // Si no, iniciamos los servidores locales
        let docker_active = build_docker && std::path::Path::new(&format!("docker-compose-bittice-{}.yml", cdc_info.entity.to_lowercase().replace(" ", "-"))).exists();
        
        if docker_active {
            crate::server::show_banner();
            crate::server::wait_for_exit(None).await?;
        } else {
            crate::server::start_all_servers().await?;
        }
    } else {
        outro("Bittice está listo. Puedes iniciarlo más tarde desde la línea de comandos.")?;
    }
    
    Ok(())
}
