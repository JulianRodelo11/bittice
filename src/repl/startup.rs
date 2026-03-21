use anyhow::Result;
use cliclack::{intro, outro, select, input, password, note, spinner, confirm};
use crate::repl::state::{App, StartupStep};
use std::thread;
use tokio::sync::mpsc;
use crate::core::cdc::CdcWorker;
use std::process::{Command, Stdio};
use std::io::{BufRead, BufReader};

pub async fn run_startup_cliclack() -> Result<Option<App>> {
    intro(" bittice ")?;

    let option: u8 = select("Seleccione el modo de operación")
        .item(0, "Conectar y sincronizar a una base de datos", "Configura una nueva conexión MySQL CDC")
        .item(1, "Usar Bittice a una base de datos ya conectada", "Entra directamente al panel de consultas")
        .interact()?;

    if option == 1 {
        let mut app = App::new();
        app.active_task = None;
        return Ok(Some(app));
    }

    // Flujo de conexión
    let host: String = input("MySQL Host").default_input("localhost").interact()?;
    let port: String = input("Puerto").default_input("3306").interact()?;
    let user: String = input("Usuario").default_input("root").interact()?;
    let pass: String = password("Contraseña").mask('*').interact()?;
    let database: String = input("Base de datos a sincronizar").placeholder("sakila").interact()?;
    let entity: String = input("Nombre de la Entidad en Bittice").default_input(&database).interact()?;

    // 1. Spinner de Sincronización
    let s = spinner();
    s.start("Iniciando motor de sincronización CDC...");

    let mut app = App::new();
    app.cdc_info.host = host;
    app.cdc_info.port = port;
    app.cdc_info.user = user;
    app.cdc_info.pass = pass;
    app.cdc_info.database = database;
    app.cdc_info.entity = entity;

    let url = format!("mysql://{}:{}@{}:{}/{}",
        app.cdc_info.user, app.cdc_info.pass,
        app.cdc_info.host, app.cdc_info.port,
        app.cdc_info.database);

    let (log_tx, mut log_rx) = mpsc::channel(100);
    app.server_log_receiver = None; // Se lo pasaremos al TUI después

    let worker_url = url.clone();
    let worker_entity = app.cdc_info.entity.clone();
    let worker_db = app.cdc_info.database.clone();

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
    app.server_log_receiver = Some(log_rx);    // 2. Preguntar por Docker
    let build_docker = confirm("¿Deseas crear una imagen de Docker con estos datos?")
        .initial_value(true)
        .interact()?;

    if build_docker {
        let s = spinner();
        s.start("Construyendo imagen Docker...");
        
        // Docker requiere nombres en minúsculas
        let image_name = format!("bittice-{}", app.cdc_info.entity).to_lowercase().replace(" ", "-");
        
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
                
                let project_name = format!("bittice-{}", app.cdc_info.entity).to_lowercase().replace(" ", "-");
                let compose_file = format!("docker-compose-{}.yml", project_name);
                
                // Ajustar URL para Docker: si es localhost, usar host.docker.internal
                let docker_mysql_host = if app.cdc_info.host == "localhost" || app.cdc_info.host == "127.0.0.1" {
                    "host.docker.internal"
                } else {
                    &app.cdc_info.host
                };

                let mysql_url = format!("mysql://{}:{}@{}:{}/{}", 
                    app.cdc_info.user, app.cdc_info.pass, 
                    docker_mysql_host, app.cdc_info.port, 
                    app.cdc_info.database);

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
    command: ["./bittice", "server", "--type", "grpc", "--port", "50051"]
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
"#, project_name, mysql_url, app.cdc_info.entity, app.cdc_info.database);

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

    // 3. Finalizar o entrar al TUI
    let go_to_tui = confirm("¿Deseas entrar al panel de consultas (Dashboard)?")
        .initial_value(true)
        .interact()?;

    if go_to_tui {
        app.active_task = None; // Ir al dashboard
        app.startup_step = StartupStep::CdcRunning;
        Ok(Some(app))
    } else {
        outro("Bittice sigue corriendo en segundo plano.")?;
        Ok(None) // Salir del programa pero dejar procesos activos si fuera necesario (o simplemente salir)
    }
}
